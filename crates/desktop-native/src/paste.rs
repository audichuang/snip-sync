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
	ReplayFailure, ReplaySkipReason,
};
use snip_core::format;
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::GitError;
use snip_core::reason::{Reason, StaleChange};
use snip_core::restore::{
	RestoreBase, RestoreBaseSuggestion, RestoreExecutionResult, RestorePlan,
	RestoreSelection,
};
use snip_core::transfer::{
	detect_clipboard_prefixes, CanonicalRootId, CommitReplayPreview,
	TransferError,
};
use snip_remote::{
	FilePastePlan, PasteError, PasteRoute, PasteTarget, RemoteError,
};

use crate::dirtree::{DirTree, TreePath, TreeSink};
use crate::i18n::{tf, Locale, Msg};
use crate::reader::{Preview, PreviewSource};
use crate::reason::reason_msg;
use crate::syntax::Language;

pub mod dest;
pub mod preview;

/// Shared retained preview tier: UI, one active paste input, one latest
/// captured request, and one result mailbox. Builder scratch is temporary.
/// The core copy cap must not exceed this budget, so it is the same number.
pub const MAX_RETAINED_PREVIEW_BYTES: usize =
	snip_core::transfer::CLIPBOARD_PAYLOAD_MAX;
// Arc's two counters plus conservative alignment padding for these types.
const ARC_ALLOWANCE: usize = 3 * size_of::<usize>();
// A BTreeSet entry's share of its node: slot, edge and padding.
const SET_ENTRY_ALLOWANCE: usize = 4 * size_of::<usize>();
/// A preview with more rows than this opens with its first-level folders folded.
const FOLD_FILES_OVER: usize = 200;

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
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		/// Set in a remote workspace: `dest` and `roots` are tree paths
		/// under its session root, and its worker plans and writes.
		remote: Option<RemotePaste>,
	},
	Remap(Remap),
	/// The previewed payload planned into another destination folder
	/// (#238). The payload is the shell's own, shared, never re-read.
	Retarget {
		dest: PathBuf,
		roots: Vec<PathBuf>,
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
	},
}

/// What a deferred re-plan changes, for its log line once it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remap {
	/// A prefix row: kept under the destination, or a candidate folder.
	Prefix { prefix: String, keep: bool },
	/// The restore-base hint: plan with the suggestion (true) or as-is.
	AdjustPaths(bool),
}

impl Remap {
	fn heap_bytes(&self) -> usize {
		match self {
			Self::Prefix { prefix, .. } => prefix.capacity(),
			Self::AdjustPaths(_) => 0,
		}
	}
}

impl PasteRequest {
	fn bytes(&self) -> usize {
		size_of::<Self>().saturating_add(match self {
			Self::Clipboard {
				text, dest, roots, ..
			} => text.capacity().saturating_add(paths_bytes(dest, roots)),
			Self::Remap(remap) => remap.heap_bytes(),
			Self::Retarget { dest, roots, .. } => paths_bytes(dest, roots),
		})
	}
}

/// Heap bytes of a destination and the repo roots sent with it: what each
/// allocated, not what it uses.
#[allow(clippy::ptr_arg)]
fn paths_bytes(dest: &PathBuf, roots: &Vec<PathBuf>) -> usize {
	roots.iter().fold(
		dest.capacity()
			.saturating_add(roots.capacity() * size_of::<PathBuf>()),
		|n, p| n.saturating_add(p.capacity()),
	)
}

pub struct PasteOutcome {
	pub result: Result<PastePreviewPlan, Msg>,
	pub remap: Option<Remap>,
	/// The plan is the answer to a `Retarget`: the destination moved.
	pub retargeted: bool,
}

impl PasteOutcome {
	fn bytes(&self) -> usize {
		match &self.result {
			Ok(plan) => plan.retained_bytes(),
			Err(err) => err.heap_bytes(),
		}
		.saturating_add(self.remap.as_ref().map_or(0, Remap::heap_bytes))
	}
}

/// One paste-specific mailbox, not a general job/resource manager. The UI's
/// lifecycle id remains occupied independently until its FinishFlag drops.
#[derive(Default)]
pub struct PastePending {
	ui_bytes: usize,
	apply_allowance: usize,
	apply_active: usize,
	/// Captured replay plans still held by selected-row reads, including
	/// cancelled reads that have not returned from their worker yet.
	detail_reads: Vec<DetailRead>,
	// U64-NOT-GEN: id of a detail read, not a landing guard
	next_detail_read: u64,
	ui_raw: Option<Weak<str>>,
	ui_commit: Option<Weak<CommitReplayPreview>>,
	seq: crate::gen::Stamp<crate::gen::PasteLane>,
	input: Option<(
		crate::gen::Stamp<crate::gen::PasteLane>,
		usize,
		Option<Weak<str>>,
	)>,
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

	/// Detail reads not already counted in the UI's bytes. A read of the
	/// UI's own commit preview costs nothing until the UI lets it go. Reads
	/// of one shared preview are charged once, so the total cannot grow when
	/// the UI drops its hold and the reads become the only owners.
	fn detail_bytes(
		&self,
		ui_commit: Option<&Weak<CommitReplayPreview>>,
	) -> usize {
		let mut counted: Vec<&Weak<CommitReplayPreview>> = Vec::new();
		let mut total = 0usize;
		for read in &self.detail_reads {
			if ui_holds(ui_commit, &read.preview)
				|| counted.iter().any(|seen| Weak::ptr_eq(seen, &read.preview))
			{
				continue;
			}
			counted.push(&read.preview);
			total = total.saturating_add(read.bytes);
		}
		total
	}

	fn total_with_ui(
		&self,
		ui_bytes: usize,
		allowance: usize,
		raw: Option<&Weak<str>>,
		commit: Option<&Weak<CommitReplayPreview>>,
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
			.saturating_add(self.detail_bytes(commit))
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
		let commit = plan
			.and_then(|p| p.commit_preview.as_ref())
			.map(Arc::downgrade);
		check_budget(
			self.total_with_ui(bytes, allowance, raw.as_ref(), commit.as_ref()),
			MAX_RETAINED_PREVIEW_BYTES,
		)?;
		self.ui_bytes = bytes;
		self.apply_allowance = allowance;
		self.ui_raw = raw;
		self.ui_commit = commit;
		Ok(())
	}

	/// How many more UI bytes `admit_ui` would accept for this state before
	/// refusing it. Zero when it is already at or over the cap.
	pub fn ui_headroom(
		&self,
		ordinary: Option<&Preview>,
		plan: &PastePreviewPlan,
		detail: Option<&Preview>,
	) -> usize {
		let bytes = preview_state_bytes(ordinary, Some(plan), detail);
		let allowance = if plan.executable() {
			plan.apply_clone_bytes()
		} else {
			0
		};
		let raw = Arc::downgrade(&plan.raw_payload);
		let commit = plan.commit_preview.as_ref().map(Arc::downgrade);
		MAX_RETAINED_PREVIEW_BYTES.saturating_sub(self.total_with_ui(
			bytes,
			allowance,
			Some(&raw),
			commit.as_ref(),
		))
	}

	pub fn invalidate(
		&mut self,
		seq: crate::gen::Stamp<crate::gen::PasteLane>,
	) {
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
				self.ui_commit.as_ref(),
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
			PasteRequest::Remap(remap) => {
				let Some(plan) = plan else { return Ok(None) };
				PasteInput::Remap {
					plan: plan.clone(),
					remap,
				}
			}
			PasteRequest::Retarget {
				dest,
				roots,
				generation,
			} => {
				let Some(plan) = plan else { return Ok(None) };
				PasteInput::Retarget {
					plan: plan.clone(),
					dest,
					roots,
					generation,
				}
			}
		};
		// A remap and a retarget plan from the shell's own payload: the
		// budget counts that one allocation once.
		let raw = match &input {
			PasteInput::Remap { plan, .. }
			| PasteInput::Retarget { plan, .. } => {
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
				state.ui_commit.as_ref(),
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

// One input is held per worker and moved, never copied; its size is counted
// by `bytes`, so a boxed variant would only move the same bytes.
#[allow(clippy::large_enum_variant)]
enum PasteInput {
	Clipboard(PasteRequest),
	Remap {
		plan: PastePreviewPlan,
		remap: Remap,
	},
	Retarget {
		plan: PastePreviewPlan,
		dest: PathBuf,
		roots: Vec<PathBuf>,
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
	},
}
impl PasteInput {
	fn bytes(&self) -> usize {
		size_of::<Self>().saturating_add(match self {
			Self::Clipboard(request) => request.bytes(),
			Self::Remap { plan, remap } => {
				plan.retained_bytes().saturating_add(remap.heap_bytes())
			}
			Self::Retarget {
				plan, dest, roots, ..
			} => plan
				.retained_bytes()
				.saturating_add(paths_bytes(dest, roots)),
		})
	}
}

pub struct PasteWorker {
	input: Option<PasteInput>,
	pool: Arc<Mutex<PastePending>>,
	cancel: CancelToken,
	seq: crate::gen::Stamp<crate::gen::PasteLane>,
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
				remote,
			}) => PasteOutcome {
				result: PastePreviewPlan::build_at(
					&text,
					&dest,
					&roots,
					generation,
					opts,
					MAX_RETAINED_PREVIEW_BYTES,
					remote,
				),
				remap: None,
				retargeted: false,
			},
			PasteInput::Remap { mut plan, remap } => {
				let result = plan.rebuild_file_plan_with(opts).map(|()| plan);
				PasteOutcome {
					result,
					remap: Some(remap),
					retargeted: false,
				}
			}
			PasteInput::Retarget {
				plan,
				dest,
				roots,
				generation,
			} => PasteOutcome {
				result: plan.retarget_with(
					&dest,
					&roots,
					generation,
					opts,
					MAX_RETAINED_PREVIEW_BYTES,
				),
				remap: None,
				retargeted: true,
			},
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
			state.ui_commit.as_ref(),
		) > MAX_RETAINED_PREVIEW_BYTES
		{
			outcome = state.ready.take().unwrap();
			drop(outcome);
			state.ready = Some(PasteOutcome {
				result: Err(preview_budget_error()),
				remap: None,
				retargeted: false,
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
	pub fn execute(self) -> Result<PasteApplyResult, ApplyRefusal> {
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
	Restore(snip_core::restore::SkipReason),
	ProtectedGitMetadata,
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
	/// Create/delete operation index, or payload file index for a commit row.
	pub op_index: usize,
	/// Commit replay only: the commit (index into the payload) this row
	/// belongs to. `None` for file restores.
	pub commit: Option<usize>,
	/// Commit replay only: the other half of a rename ("→ new" on the
	/// deleted old path, "← old" on the written new path).
	pub rename_note: Option<String>,
}

impl crate::dirtree::TreePath for PasteItem {
	fn tree_path(&self) -> &str {
		&self.path
	}
}

impl PasteItem {
	pub fn is_delete(&self) -> bool {
		matches!(self.op, PlannedOp::Delete)
	}

	pub fn writes_content(&self) -> bool {
		matches!(self.op, PlannedOp::Create | PlannedOp::Overwrite)
	}

	/// The deleted old path of a commit's rename. It stands beside the new
	/// path in the tree, so its row names its own full path.
	pub fn is_rename_source(&self) -> bool {
		self.is_delete() && self.rename_note.is_some()
	}

	/// An existing destination file that the user may choose to overwrite.
	/// A skipped row is never written, so it never asks.
	pub fn overwritable(&self) -> bool {
		self.dest_exists
			&& matches!(self.op, PlannedOp::Create | PlannedOp::Overwrite)
	}

	pub fn planned_skip(&self) -> bool {
		self.commit.is_none() && matches!(self.op, PlannedOp::Skip(_))
	}

	pub fn action(&self) -> RowAction {
		if self.planned_skip() {
			if let PlannedOp::Skip(reason) = self.op {
				return RowAction::Skip(reason);
			}
		}
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

/// Key of a destination root in the preview tree.
pub type RootKey = PathBuf;

/// A folder row of the paste tree: its root and its path under it ("" is
/// the root row itself). A commit replay lists each commit's own folders, so
/// a folder named like another commit's is a different row: `commit` says
/// whose it is (`None` in a file paste).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirKey {
	pub commit: Option<usize>,
	pub root: RootKey,
	pub path: String,
}

/// Where the paste cursor is. Commit replays only ever have `Item`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteCursor {
	Item(usize),
	Dir(DirKey),
}

/// A folder's include box. `Locked`: nothing under it can be toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
	All,
	Partial,
	Empty,
	Locked,
}

/// What a folder row counts over the rows under it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirSummary {
	/// Every row under it, skipped ones included.
	pub files: usize,
	/// Rows whose include box can be toggled (not skipped when planned).
	pub toggleable: usize,
	/// Toggleable rows that are included.
	pub included: usize,
}

impl DirSummary {
	pub fn tick(&self) -> Tick {
		if self.toggleable == 0 {
			Tick::Locked
		} else if self.included == self.toggleable {
			Tick::All
		} else if self.included == 0 {
			Tick::Empty
		} else {
			Tick::Partial
		}
	}
}

/// What the arrow keys do at the cursor. The session applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeStep {
	Nothing,
	Fold(DirKey),
	Unfold(DirKey),
	Move(PasteCursor),
}

/// One row of the paste change tree (built per frame, never retained).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteNode {
	/// A destination root; `ord` is its position on screen (probe ids).
	Root {
		key: DirKey,
		ord: usize,
		label: String,
		sum: DirSummary,
		collapsed: bool,
	},
	/// A directory under a root, as one compressed path ("src/app").
	Dir {
		key: DirKey,
		ord: usize,
		name: String,
		depth: usize,
		sum: DirSummary,
		collapsed: bool,
	},
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

/// 1023 B, 1.5 KB, 2.1 MB (binary units, one decimal).
// U64-NOT-GEN: a byte count
pub(crate) fn human_bytes(n: u64) -> String {
	const K: f64 = 1024.0;
	if n < 1024 {
		format!("{n} B")
	} else if (n as f64) < K * K {
		format!("{:.1} KB", n as f64 / K)
	} else {
		format!("{:.1} MB", n as f64 / (K * K))
	}
}

/// Eight-character SHAs; past `max`, the rest as "+N".
pub(crate) fn short_shas(shas: &[String], max: usize) -> String {
	let mut out: Vec<String> = shas
		.iter()
		.take(max)
		.map(|s| s.chars().take(8).collect())
		.collect();
	if shas.len() > max {
		out.push(format!("+{}", shas.len() - max));
	}
	out.join(", ")
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
				[short_shas(&self.created_commits, 3)],
			);
		}
		let f = &self.files;
		let counts = [
			f.created_count,
			f.overwritten_count,
			f.total_skipped(),
			f.deleted_count,
			f.errors.len(),
		]
		.map(|n| n.to_string());
		if f.errors.is_empty() {
			if f.created_count + f.overwritten_count + f.deleted_count == 0
				&& f.total_skipped() > 0
			{
				return Msg::new(
					"status_paste_skipped",
					[f.total_skipped().to_string()],
				);
			}
			return Msg::new("status_paste_done", counts);
		}
		let failures = self.failures();
		let first = restore_failure_msg(&failures[0]);
		let mut args = counts.to_vec();
		args.push(String::new());
		if failures.len() == 1 {
			return Msg::with_msg_arg("status_paste_partial", args, 5, first);
		}
		// The first is the headline's own argument; every other failure
		// follows it, path and cause, worded when drawn.
		args.push(String::new());
		let rest = failures[1..].iter().map(restore_failure_msg).collect();
		Msg::with_msg_arg("status_paste_partial_more", args, 5, first)
			.and_list_arg(6, rest)
	}

	/// The failures as data. A worker that predates typed failures sent
	/// only its text, kept here as the data trailing a generic headline.
	fn failures(&self) -> Vec<snip_core::restore::RestoreFailure> {
		let f = &self.files;
		if f.failures.len() == f.errors.len() {
			return f.failures.clone();
		}
		f.errors
			.iter()
			.map(|text| snip_core::restore::RestoreFailure {
				path: String::new(),
				reason: Some(snip_core::reason::Reason::Io {
					detail: snip_core::reason::Datum::old_peer_text(
						text.clone(),
					),
				}),
			})
			.collect()
	}
}

/// One failed file: its path, then why.
fn restore_failure_msg(f: &snip_core::restore::RestoreFailure) -> Msg {
	let why = f
		.reason
		.as_ref()
		.map_or_else(|| Msg::new("restore_err_unknown", []), reason_msg);
	if f.path.is_empty() {
		return why;
	}
	Msg::with_msg_arg(
		"restore_err_file",
		[f.path.clone(), String::new()],
		1,
		why,
	)
}

/// A paste into a remote workspace: its worker plans and writes through
/// [`PasteTarget::Remote`] with the same engine, on its own disk. Here only
/// the tree path's spelling and display live.
#[derive(Clone)]
pub struct RemotePaste {
	pub session: crate::remote::RemoteSession,
}

impl std::fmt::Debug for RemotePaste {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("RemotePaste")
			.field(&self.session.root)
			.finish()
	}
}

impl RemotePaste {
	/// The workspace folder (relative to the workspace) of a tree path.
	fn rel(&self, path: &Path) -> Result<String, Msg> {
		crate::remote::remote_rel(&self.session.root, path).ok_or_else(|| {
			Msg::with_key_arg(
				"paste_err_destination",
				[self.shown(path), "err_not_under_workspace".into()],
				1,
			)
		})
	}

	/// A tree path as the user knows it: `host:/folder/…`.
	pub fn shown(&self, path: &Path) -> String {
		crate::remote::display_path(Some(&self.session), path)
	}
}

/// A failed Apply: what the UI says, and the token `PASTE_STALE_DETECTED`
/// prints (an e2e contract that no dictionary rename may change). Built only
/// with a token, so a new refusal on the Apply path cannot fall back to
/// `stale_modified` unnoticed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRefusal {
	pub msg: Msg,
	pub token: ApplyToken,
}

/// Every token an Apply failure prints on `PASTE_STALE_DETECTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyToken {
	StaleCreated,
	StaleDeleted,
	StaleModified,
	PasteRepoBusy,
	PasteOutcomeUnknown,
	PasteErrDestination,
	PasteErrPlan,
	ErrorOpenRepo,
	CommitReplayRefused,
	CommitReplayPartial,
	CommitReplayPartialRefused,
	CommitSubsetRejected,
	CommitOverwriteRequired,
	MappingRequired,
}

impl ApplyToken {
	/// The spelling drivers read. Never changes with the dictionary.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::StaleCreated => "stale_created",
			Self::StaleDeleted => "stale_deleted",
			Self::StaleModified => "stale_modified",
			Self::PasteRepoBusy => "paste_repo_busy",
			Self::PasteOutcomeUnknown => "paste_outcome_unknown",
			Self::PasteErrDestination => "paste_err_destination",
			Self::PasteErrPlan => "paste_err_plan",
			Self::ErrorOpenRepo => "error_open_repo",
			Self::CommitReplayRefused => "commit_replay_refused",
			Self::CommitReplayPartial => "commit_replay_partial",
			Self::CommitReplayPartialRefused => "commit_replay_partial_refused",
			Self::CommitSubsetRejected => "commit_subset_rejected",
			Self::CommitOverwriteRequired => "commit_overwrite_required",
			Self::MappingRequired => "mapping_required",
		}
	}

	/// A reason's own token: a created or deleted destination file names
	/// itself, a busy repository is `paste_repo_busy`, anything else reads
	/// `stale_modified`. Exhaustive over `Reason`, no `_`: a new reason
	/// decides.
	pub fn of_reason(r: &Reason) -> Self {
		match r {
			Reason::RepoBusy => Self::PasteRepoBusy,
			Reason::DestinationChanged { change } => match change {
				StaleChange::Created { .. } => Self::StaleCreated,
				StaleChange::Deleted { .. } => Self::StaleDeleted,
				StaleChange::Modified { .. }
				| StaleChange::Repository
				| StaleChange::Preview => Self::StaleModified,
			},
			Reason::Refused { .. }
			| Reason::Start { .. }
			| Reason::Disconnected { .. }
			| Reason::WorkerClosed
			| Reason::Protocol
			| Reason::WorkerTooOld { .. }
			| Reason::TimedOut
			| Reason::Cancelled
			| Reason::GitMissing
			| Reason::GitFailed { .. }
			| Reason::GitBusy
			| Reason::WorkerNeedsRestart
			| Reason::TooLarge
			| Reason::PatchTooLarge
			| Reason::VerifyLimit
			| Reason::NotARepository { .. }
			| Reason::InsideParentRepo { .. }
			| Reason::InvalidRevision { .. }
			| Reason::Shallow { .. }
			| Reason::OutsideShare
			| Reason::Internal { .. }
			| Reason::Io { .. }
			| Reason::NotFound { .. }
			| Reason::PermissionDenied { .. }
			| Reason::EmptySelection
			| Reason::UnsafePath { .. }
			| Reason::NotRegularFile { .. }
			| Reason::OutsideRoot { .. }
			| Reason::Collision { .. }
			| Reason::StagedAndWorking { .. }
			| Reason::AmbiguousRoots { .. }
			| Reason::PayloadTooLarge { .. }
			| Reason::SourceChanged { .. }
			| Reason::NewDirsNotConfirmed { .. }
			| Reason::CrossRepoCommits
			| Reason::Discontinuous { .. }
			| Reason::NotEnoughCommits { .. }
			| Reason::NotCommitPayload
			| Reason::InvalidPayload { .. }
			| Reason::OutsideScope { .. }
			| Reason::Unverifiable { .. }
			| Reason::SubmoduleNotCheckedOut
			| Reason::SubmoduleUnreadable { .. }
			| Reason::ScanErrorsOmitted { .. }
			| Reason::DepthLimitedOmitted { .. } => Self::StaleModified,
		}
	}
}

impl ApplyRefusal {
	/// A refusal worded from a reason, with the reason's own token.
	fn of_reason(r: &Reason) -> Self {
		Self {
			msg: reason_msg(r),
			token: ApplyToken::of_reason(r),
		}
	}

	/// A refusal the app words itself, under the token of that wording.
	fn app(msg: Msg, token: ApplyToken) -> Self {
		Self { msg, token }
	}
}

/// Another tab, process or worker holds the repository's write lock.
fn repo_busy() -> Msg {
	Msg::new("paste_repo_busy", [])
}

/// The connection was lost after an Apply was sent: the worker may have
/// written it. Refresh shows what landed.
fn outcome_unknown() -> Msg {
	Msg::new("paste_outcome_unknown", [])
}

/// The paste target for `dest` (a tree path): this machine, or the remote
/// workspace's worker.
fn paste_target(
	remote: Option<&RemotePaste>,
	dest: &Path,
) -> Result<PasteTarget, Msg> {
	Ok(match remote {
		// The app never makes folders that do not exist: `allow_new_dirs`
		// stays off, so `bound` is not read.
		None => PasteTarget::Local {
			dest: dest.to_path_buf(),
			bound: None,
		},
		Some(r) => PasteTarget::Remote {
			client: r.session.client.clone(),
			workspace: r.session.workspace.id.clone(),
			dest: r.rel(dest)?,
		},
	})
}

/// A destination the paste could not resolve or reach, named as the user
/// knows it. Worded when drawn, in the language then in force: a plan
/// outlives a switch of language.
fn destination_msg(
	remote: Option<&RemotePaste>,
	path: &Path,
	err: PasteError,
) -> Msg {
	// A local folder that did not resolve is named by its own path: a
	// prefix's folder is not the destination.
	if let PasteError::Destination { path, source } = &err {
		return destination_error(path, source);
	}
	let shown =
		remote.map_or_else(|| path.display().to_string(), |r| r.shown(path));
	Msg::with_msg_arg(
		"paste_err_destination",
		[shown, String::new()],
		1,
		reason_msg(&err.reason()),
	)
}

/// A file Apply's failure, worded as before for each target.
fn file_apply_msg(
	remote: Option<&RemotePaste>,
	dest: &Path,
	err: PasteError,
) -> Msg {
	file_apply_refusal(remote, dest, err).msg
}

/// [`file_apply_msg`] with the token the failure prints.
fn file_apply_refusal(
	remote: Option<&RemotePaste>,
	dest: &Path,
	err: PasteError,
) -> ApplyRefusal {
	if err.is_repo_busy() {
		return ApplyRefusal::app(repo_busy(), ApplyToken::PasteRepoBusy);
	}
	match err {
		PasteError::OutcomeUnknown(_) => ApplyRefusal::app(
			outcome_unknown(),
			ApplyToken::PasteOutcomeUnknown,
		),
		PasteError::Local(e) => ApplyRefusal::of_reason(&e.reason()),
		other if other.is_stale() => ApplyRefusal::of_reason(&other.reason()),
		other => ApplyRefusal::app(
			destination_msg(remote, dest, other),
			ApplyToken::PasteErrDestination,
		),
	}
}

/// A commit check's (`applying` false) or replay's failure, worded as before.
fn commit_msg(
	remote: Option<&RemotePaste>,
	dest: &Path,
	err: PasteError,
	applying: bool,
) -> Msg {
	commit_refusal(remote, dest, err, applying).msg
}

/// [`commit_msg`] with the token the failure prints.
fn commit_refusal(
	remote: Option<&RemotePaste>,
	dest: &Path,
	err: PasteError,
	applying: bool,
) -> ApplyRefusal {
	if err.is_stale() {
		return ApplyRefusal::of_reason(&err.reason());
	}
	let open_repo = |reason: &Reason| {
		ApplyRefusal::app(
			Msg::with_msg_arg(
				"error_open_repo",
				[String::new()],
				0,
				reason_msg(reason),
			),
			ApplyToken::ErrorOpenRepo,
		)
	};
	match err {
		PasteError::Local(TransferError::Git(GitError::WorktreeBusy {
			..
		})) => ApplyRefusal::app(repo_busy(), ApplyToken::PasteRepoBusy),
		PasteError::Local(TransferError::Git(e)) if applying => {
			open_repo(&e.reason())
		}
		PasteError::Local(e) => ApplyRefusal::of_reason(&e.reason()),
		// Kept on purpose (design Risk 1): any other worker refusal of a
		// replay, Busy included, reads as a repository error, not as
		// paste_repo_busy. A refusal is worded by its reason, never by the
		// worker's text.
		PasteError::Remote(e @ RemoteError::Refused { .. }) => {
			open_repo(&e.reason())
		}
		PasteError::OutcomeUnknown(_) => ApplyRefusal::app(
			outcome_unknown(),
			ApplyToken::PasteOutcomeUnknown,
		),
		other => ApplyRefusal::app(
			destination_msg(remote, dest, other),
			ApplyToken::PasteErrDestination,
		),
	}
}

// U64-NOT-GEN: content digest of the roots, not a counter
pub(crate) fn roots_digest(roots: &[PathBuf]) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	roots.hash(&mut hasher);
	hasher.finish()
}

#[derive(Debug, Clone)]
pub struct PastePreviewPlan {
	/// In a remote workspace, the tree path the user pasted into.
	pub destination: PathBuf,
	/// Shared file plan: the engine's plan, plus the worker's digest in a remote workspace.
	pub import_plan: Option<Arc<FilePastePlan>>,
	/// Whole-commit replay. File checkboxes do not rewrite a subset.
	pub commit_preview: Option<Arc<CommitReplayPreview>>,
	/// The worker's retained commit preview (protocol 8), shared by clones.
	pub commit_prepared: snip_remote::PreparedReplay,
	pub whole_commit: bool,
	/// Commits whose file rows are folded away (commit replay only).
	pub collapsed_commits: BTreeSet<usize>,
	/// Folders whose rows are folded away (file mode). Keyed by path, so a
	/// remap keeps the folds of folders that still exist.
	pub collapsed_dirs: BTreeSet<DirKey>,
	/// The folder the cursor is on. `Some` takes precedence over
	/// `selected_item_idx`.
	pub cursor_dir: Option<DirKey>,
	/// The first preview with rows has had its large folders folded.
	fold_seeded: bool,
	pub prefix_choices: Vec<PrefixChoice>,
	/// Digest of the repo roots the candidates were last built from, so a
	/// landing that brings the same set skips the canonicalize.
	// U64-NOT-GEN: content digest of the roots, not a counter
	seen_roots: u64,
	/// The restore-base suggestion the last plan heard (local engine or the
	/// worker). Only kept when there are no prefix rows: see `restore_hint`.
	restore_base: Option<RestoreBaseSuggestion>,
	/// The hint's [Adjust paths]: plan with `restore_base`. Off on every
	/// paste, so nothing is adjusted unasked.
	pub adjust_paths: bool,
	/// Shared so starting an apply does not copy every file body.
	pub plan: Arc<RestorePlan>,
	pub items: Vec<PasteItem>,
	pub selected_item_idx: usize,
	pub is_applying: bool,
	pub generation_snapshot: crate::gen::Stamp<crate::gen::RepoLoadLane>,
	pub error: Option<Msg>,
	/// Shared so a clone for a background rebuild or an apply is cheap.
	raw_payload: Arc<str>,
	/// Set in a remote workspace. `import_plan` and `commit_preview` then
	/// hold the worker's plan, with the worker's paths.
	pub remote: Option<RemotePaste>,
}

/// Heap bytes of a suggestion, charged to the plan that keeps it.
fn suggestion_heap(s: &RestoreBaseSuggestion) -> usize {
	s.label.capacity()
		+ match &s.base {
			RestoreBase::Strip { segment } => segment.capacity(),
			RestoreBase::Add { prefix } => prefix.capacity(),
		}
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirControl {
	Row,
	Toggle,
	Include,
}

/// Element and probe id of a folder row's control:
/// `paste-dir[-toggle|-include]:<ord>:<rel>`. `ord` is the root's position
/// on screen (a name can repeat or hold `:`); `rel` is the folder's path
/// under it, empty for the root row itself.
pub fn dir_control_id(kind: DirControl, ord: usize, rel: &str) -> String {
	let kind = match kind {
		DirControl::Row => "",
		DirControl::Toggle => "-toggle",
		DirControl::Include => "-include",
	};
	format!("paste-dir{kind}:{ord}:{rel}")
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
		LayoutConflict::CaseAlias => "reason_refused_case_alias",
	}
}

/// Why a replay stopped. A worker that sent no reason (an old one) is worded
/// by the generic I/O cause: the failure's English text is never shown.
fn replay_failure_reason(fail: &ReplayFailure) -> snip_core::reason::Reason {
	fail.reason
		.clone()
		.unwrap_or(snip_core::reason::Reason::Refused {
			code: snip_remote::ErrorCode::Io,
		})
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
		LayoutConflict::CaseAlias => "reason_refusal_cause_case_alias",
	}
}

#[cfg(test)]
fn header_label(op: PlannedOp) -> &'static str {
	match op {
		PlannedOp::Create => "CREATE",
		PlannedOp::Overwrite => "OVERWRITE",
		PlannedOp::Delete => "DELETE",
		PlannedOp::Skip(_) => "SKIP",
		PlannedOp::CommitRefused(_) => "REFUSED",
	}
}

/// A root's last name, read as written with `/` and `\` both separating:
/// a worker's path is not taken apart by this machine's rules (on Unix `\`
/// is part of a name, and a Windows worker's whole root was one name).
fn root_name(path: &Path) -> String {
	let text = path.to_string_lossy();
	match text
		.trim_end_matches(['/', '\\'])
		.rsplit(['/', '\\'])
		.next()
	{
		Some(name) if !name.is_empty() => name.to_string(),
		_ => path.display().to_string(),
	}
}

/// `path` as text with `/` separators and no trailing one: how a worker's
/// paths are compared here, whatever OS wrote them.
fn slashed(path: &Path) -> String {
	let text = path.to_string_lossy().replace('\\', "/");
	text.trim_end_matches('/').to_string()
}

/// A destination that could not be opened or captured, named by path so the
/// status says which directory was refused (the OS error alone does not).
pub(crate) fn destination_error(dest: &Path, err: &std::io::Error) -> Msg {
	Msg::with_msg_arg(
		"paste_err_destination",
		[dest.display().to_string(), String::new()],
		1,
		reason_msg(&snip_core::reason::io_reason(err, "")),
	)
}

fn tally<'a>(items: impl Iterator<Item = &'a PasteItem>) -> PasteCounts {
	let mut c = PasteCounts::default();
	for it in items {
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

/// The items of one destination root.
struct RootGroup {
	root: RootKey,
	label: String,
	/// Every item of the root.
	all: Vec<usize>,
	/// Items with a place under the root.
	placed: Vec<usize>,
	/// Items skipped when planned: they only have a raw path.
	skipped: Vec<usize>,
}

/// The root a path lies in. With one root inside another (a prefix mapped to
/// a folder of the destination) it is the inner one, where a write to that
/// path would be shown too.
/// Compared as text, as [`plan_root_of`] does, so a worker's Windows paths
/// read the same on any master.
fn innermost_root<'a>(
	roots: &'a [PathBuf],
	path: &Path,
) -> Option<&'a PathBuf> {
	let path = slashed(path);
	roots
		.iter()
		.map(|r| (r, slashed(r)))
		.filter(|(_, r)| {
			path.strip_prefix(r.as_str())
				.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
		})
		.max_by_key(|(_, r)| r.len())
		.map(|(r, _)| r)
}

/// The root a planned file's `relative` path is relative to: the one of
/// `roots` that is `absolute` with `relative` taken off its end. Compared
/// as text with `/` separators, so a worker's Windows paths read the same on
/// any master. `None` when no root has that spelling.
fn plan_root_of<'a>(
	roots: &'a [PathBuf],
	absolute: &Path,
	relative: &str,
) -> Option<&'a PathBuf> {
	let absolute = slashed(absolute);
	// The name must start right after a separator: `sub` + `g.txt` is not
	// the file `subg.txt`.
	let base = absolute
		.strip_suffix(relative.trim_matches('/'))?
		.strip_suffix('/')?;
	roots.iter().find(|r| slashed(r) == base)
}

/// Whether `path` lies inside folder `dir` (not merely shares its start).
fn path_under(path: &str, dir: &str) -> bool {
	path.strip_prefix(dir)
		.is_some_and(|rest| rest.starts_with('/'))
}

/// Turns the shared directory tree's rows into the paste tree's nodes.
struct PasteSink<'a> {
	plan: &'a PastePreviewPlan,
	root: &'a RootKey,
	ord: usize,
	rows: &'a mut Vec<PasteNode>,
}

impl TreeSink for PasteSink<'_> {
	fn dir(
		&mut self,
		path: &str,
		name: &str,
		members: &[usize],
		depth: usize,
	) -> bool {
		let key = DirKey {
			commit: None,
			root: self.root.clone(),
			path: path.to_string(),
		};
		let collapsed = self.plan.collapsed_dirs.contains(&key);
		self.rows.push(PasteNode::Dir {
			key,
			ord: self.ord,
			name: name.to_string(),
			depth,
			sum: self.plan.summary(members),
			collapsed,
		});
		!collapsed
	}

	fn file(&mut self, ix: usize, depth: usize) {
		self.rows.push(PasteNode::File(ix, depth));
	}
}

/// A commit row's place in the tree, for [`DirTree`].
struct Placed<'a>(&'a str);

impl TreePath for Placed<'_> {
	fn tree_path(&self) -> &str {
		self.0
	}
}

/// Turns the directory tree of one commit's rows into paste nodes. The tree
/// sees the commit's rows only, so its indices start at `base` in `items`.
struct CommitSink<'a> {
	plan: &'a PastePreviewPlan,
	commit: usize,
	base: usize,
	rows: &'a mut Vec<PasteNode>,
}

impl TreeSink for CommitSink<'_> {
	fn dir(
		&mut self,
		path: &str,
		name: &str,
		members: &[usize],
		depth: usize,
	) -> bool {
		let key = DirKey {
			commit: Some(self.commit),
			root: self.plan.items[self.base].dest_root.clone(),
			path: path.to_string(),
		};
		let collapsed = self.plan.collapsed_dirs.contains(&key);
		let members: Vec<usize> =
			members.iter().map(|&m| self.base + m).collect();
		self.rows.push(PasteNode::Dir {
			key,
			ord: self.commit,
			name: name.to_string(),
			depth,
			sum: self.plan.summary(&members),
			collapsed,
		});
		!collapsed
	}

	fn file(&mut self, ix: usize, depth: usize) {
		self.rows.push(PasteNode::File(self.base + ix, depth));
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

/// A selected replay row shares its captured plan; it never clones payload
/// text or computes replay state in the desktop front end.
pub(crate) struct CommitDiffRequest {
	pub target: PasteTarget,
	pub preview: Arc<CommitReplayPreview>,
	pub commit: usize,
	pub file: usize,
	pub side: snip_core::transfer::ReplayFileSide,
	// Declared last: the retained preview drops before its charge.
	_charge: DetailReadCharge,
}

/// A live detail read of a captured replay preview. Its bytes are charged once
/// per distinct preview, unless the UI holds that Arc, which the UI's bytes
/// already count.
struct DetailRead {
	// U64-NOT-GEN: id of a detail read, not a landing guard
	id: u64,
	preview: Weak<CommitReplayPreview>,
	bytes: usize,
}

/// True when the UI's live commit preview is this exact Arc.
fn ui_holds(
	ui: Option<&Weak<CommitReplayPreview>>,
	preview: &Weak<CommitReplayPreview>,
) -> bool {
	match (ui.and_then(Weak::upgrade), preview.upgrade()) {
		(Some(ui), Some(held)) => Arc::ptr_eq(&ui, &held),
		_ => false,
	}
}

struct DetailReadCharge {
	pool: Arc<Mutex<PastePending>>,
	// U64-NOT-GEN: id of a detail read, not a landing guard
	id: u64,
}

impl DetailReadCharge {
	fn admit(
		pool: &Arc<Mutex<PastePending>>,
		preview: &Arc<CommitReplayPreview>,
	) -> Result<Self, Msg> {
		let bytes = size_of::<CommitReplayPreview>()
			.saturating_add(ARC_ALLOWANCE)
			.saturating_add(preview.retained_heap_bytes());
		let weak = Arc::downgrade(preview);
		let mut state = lock_pending(pool);
		// A read of a preview that is already charged (by the UI or by an
		// earlier read) adds nothing to the total.
		let charged = ui_holds(state.ui_commit.as_ref(), &weak)
			|| state
				.detail_reads
				.iter()
				.any(|read| Weak::ptr_eq(&read.preview, &weak));
		let extra = if charged { 0 } else { bytes };
		check_budget(
			state
				.total_with_ui(
					state.ui_bytes,
					state.apply_allowance,
					state.ui_raw.as_ref(),
					state.ui_commit.as_ref(),
				)
				.saturating_add(extra),
			MAX_RETAINED_PREVIEW_BYTES,
		)?;
		let id = state.next_detail_read;
		state.next_detail_read = id.wrapping_add(1);
		state.detail_reads.push(DetailRead {
			id,
			preview: weak,
			bytes,
		});
		Ok(Self {
			pool: pool.clone(),
			id,
		})
	}
}

impl Drop for DetailReadCharge {
	fn drop(&mut self) {
		let mut state = lock_pending(&self.pool);
		let at = state
			.detail_reads
			.iter()
			.position(|read| read.id == self.id)
			.expect("detail read charge released once");
		state.detail_reads.swap_remove(at);
	}
}

impl PastePreviewPlan {
	/// A handle that stays equal to itself for as long as this payload is
	/// the one previewed (a retarget keeps it, a new paste does not).
	pub fn payload_token(&self) -> std::sync::Weak<str> {
		Arc::downgrade(&self.raw_payload)
	}

	pub(crate) fn commit_diff_request(
		&self,
		idx: usize,
		pool: &Arc<Mutex<PastePending>>,
	) -> Result<Option<CommitDiffRequest>, Msg> {
		let Some(item) = self.items.get(idx) else {
			return Ok(None);
		};
		let Some(commit) = item.commit else {
			return Ok(None);
		};
		let Some(preview) = self.commit_preview.as_ref() else {
			return Ok(None);
		};
		let charge = DetailReadCharge::admit(pool, preview)?;
		let side = if item.is_delete() && item.rename_note.is_some() {
			snip_core::transfer::ReplayFileSide::RenamedFrom
		} else {
			snip_core::transfer::ReplayFileSide::Target
		};
		Ok(Some(CommitDiffRequest {
			target: paste_target(self.remote.as_ref(), &self.destination)?,
			preview: preview.clone(),
			commit,
			file: item.op_index,
			side,
			_charge: charge,
		}))
	}

	pub fn detail_preview(&self) -> Option<Preview> {
		if self.cursor_dir.is_some() {
			return None;
		}
		self.detail_at(self.selected_item_idx)
	}

	pub(crate) fn detail_at(&self, idx: usize) -> Option<Preview> {
		self.items
			.get(idx)
			.filter(|i| i.commit.is_some() || !i.is_delete())
			.map(|i| {
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
			.saturating_add(self.dir_state_bytes())
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
				.saturating_add(size_of::<FilePastePlan>() + ARC_ALLOWANCE)
				.saturating_add(plan.retained_heap_bytes());
		}
		if let Some(plan) = &self.commit_preview {
			bytes = bytes
				.saturating_add(
					size_of::<CommitReplayPreview>() + ARC_ALLOWANCE,
				)
				.saturating_add(plan.retained_heap_bytes())
				.saturating_add(self.commit_prepared.retained_heap_bytes());
		}
		if let Some(hint) = &self.restore_base {
			bytes = bytes.saturating_add(suggestion_heap(hint));
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

	/// The folds and the folder cursor: the keys' buffers and a conservative
	/// share of the set's nodes.
	fn dir_state_bytes(&self) -> usize {
		let key =
			|k: &DirKey| k.root.capacity().saturating_add(k.path.capacity());
		self.collapsed_dirs.iter().fold(
			self.cursor_dir.as_ref().map_or(0, key),
			|bytes, k| {
				bytes
					.saturating_add(size_of::<DirKey>())
					.saturating_add(key(k))
					.saturating_add(SET_ENTRY_ALLOWANCE)
			},
		)
	}

	fn apply_clone_bytes(&self) -> usize {
		let mut bytes = size_of::<Self>()
			.saturating_add(self.destination.capacity())
			.saturating_add(
				self.prefix_choices.capacity() * size_of::<PrefixChoice>(),
			)
			.saturating_add(self.items.capacity() * size_of::<PasteItem>())
			.saturating_add(self.dir_state_bytes());
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
		bytes
			.saturating_add(
				self.restore_base.as_ref().map_or(0, suggestion_heap),
			)
			.saturating_add(self.error.as_ref().map_or(0, Msg::heap_bytes))
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

	/// Adds repos discovered after this preview opened to every prefix
	/// choice's candidates. Called by `repos_changed` on every change of the
	/// repo list, and on each transition that returns an open preview to
	/// idle (a job landing, a failed Apply). Existing candidates keep their index and made
	/// choices stay, so nothing the user did is invalidated. Returns whether
	/// any candidate was added. Does nothing, and touches no file system,
	/// when there is no mapping to extend or the repo set is the one the
	/// plan last saw. A preview opened before the scan keeps the workspace
	/// root as its primary destination (accepted): only candidates grow.
	pub fn add_candidates(&mut self, known_roots: &[PathBuf]) -> bool {
		match self.new_roots(known_roots) {
			Some(found) => self.extend_candidates(&found).is_some(),
			None => false,
		}
	}

	/// The digest of the repo roots this plan last saw.
	// U64-NOT-GEN: content digest of the roots, not a counter
	pub(crate) fn seen_roots(&self) -> u64 {
		self.seen_roots
	}

	/// The repo roots this plan has not seen, canonical (or as the worker
	/// reported them, remotely). `None`, with no file system touched, when
	/// there is no mapping to extend or the set is the one last seen. The
	/// digest moves on.
	pub(crate) fn new_roots(
		&mut self,
		known_roots: &[PathBuf],
	) -> Option<Vec<PathBuf>> {
		if self.prefix_choices.is_empty() {
			return None;
		}
		let digest = roots_digest(known_roots);
		if digest == self.seen_roots {
			return None;
		}
		self.seen_roots = digest;
		Some(if self.remote.is_some() {
			known_roots.to_vec()
		} else {
			canonical_dirs(known_roots)
		})
	}

	/// What [`Self::extend_candidates`] would add to the retained bytes of
	/// the plan, counted the way [`Self::retained_bytes`] counts, without
	/// allocating a path. Stops counting once it passes `limit`, so the work
	/// is bounded by what the budget admits, not by prefixes x roots.
	pub(crate) fn candidate_growth_bytes(
		&self,
		found: &[PathBuf],
		limit: usize,
	) -> usize {
		let mut total = 0usize;
		for choice in &self.prefix_choices {
			#[cfg(test)]
			GROWTH_CHOICES_VISITED.with(|c| c.set(c.get() + 1));
			let have: std::collections::HashSet<&PathBuf> =
				choice.candidates.iter().collect();
			let mut seen: std::collections::HashSet<&PathBuf> =
				std::collections::HashSet::new();
			let mut added = 0usize;
			let mut path_bytes = 0usize;
			for root in found {
				if !have.contains(root) && seen.insert(root) {
					added += 1;
					path_bytes =
						path_bytes.saturating_add(root.as_os_str().len());
					// Cheap early exit on a runaway choice.
					if path_bytes > limit {
						return usize::MAX;
					}
				}
			}
			if added == 0 {
				continue;
			}
			let slots =
				(choice.candidates.len() + added) * size_of::<PathBuf>();
			let grown = slots
				.saturating_sub(
					choice.candidates.capacity() * size_of::<PathBuf>(),
				)
				.saturating_add(path_bytes);
			total = total.saturating_add(grown);
			if total > limit {
				return usize::MAX;
			}
		}
		// An executable plan is charged again in its apply clone.
		if self.executable() {
			total = total.saturating_mul(2);
		}
		total
	}

	/// Adds `found` to every choice's candidates, on exact-size copies.
	/// Returns each grown choice's previous allocation (`None` for one left
	/// as is), in order, so a refused admission can put exactly that back; `None` when nothing was added
	/// (and nothing was copied).
	pub(crate) fn extend_candidates(
		&mut self,
		found: &[PathBuf],
	) -> Option<Vec<Option<Vec<PathBuf>>>> {
		let mut grown: Vec<Option<Vec<PathBuf>>> =
			Vec::with_capacity(self.prefix_choices.len());
		let mut any = false;
		for choice in &self.prefix_choices {
			let have: std::collections::HashSet<&PathBuf> =
				choice.candidates.iter().collect();
			let mut fresh: Vec<&PathBuf> = Vec::new();
			let mut seen: std::collections::HashSet<&PathBuf> =
				std::collections::HashSet::new();
			for root in found {
				if !have.contains(root) && seen.insert(root) {
					fresh.push(root);
				}
			}
			if fresh.is_empty() {
				grown.push(None);
				continue;
			}
			any = true;
			let mut next =
				Vec::with_capacity(choice.candidates.len() + fresh.len());
			next.extend(choice.candidates.iter().cloned());
			next.extend(fresh.into_iter().cloned());
			grown.push(Some(next));
		}
		if !any {
			return None;
		}
		Some(
			self.prefix_choices
				.iter_mut()
				.zip(grown)
				.map(|(choice, next)| {
					next.map(|next| {
						std::mem::replace(&mut choice.candidates, next)
					})
				})
				.collect(),
		)
	}

	pub fn mapping_ready(&self) -> bool {
		self.prefix_choices
			.iter()
			.all(|c| c.keep_relative || c.destination.is_some())
	}

	/// A destination path as shown: `host:/folder/…` in a remote workspace.
	pub fn shown(&self, path: &Path) -> String {
		match &self.remote {
			Some(remote) => remote.shown(path),
			None => path.display().to_string(),
		}
	}

	pub fn build_from_clipboard_text(
		raw_text: &str,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
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
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
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
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
	) -> Result<Self, Msg> {
		Self::build_at(
			raw_text,
			dest,
			known_roots,
			generation,
			opts,
			limit,
			None,
		)
	}

	/// Builds the preview, planned by `remote`'s worker when set.
	fn build_at(
		raw_text: &str,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
		remote: Option<RemotePaste>,
	) -> Result<Self, Msg> {
		// arboard has already allocated the OS clipboard String. Refuse before
		// parsing it or retaining any extra copy; never truncate a payload.
		check_budget(raw_text.len(), limit)?;
		Self::build_shared(
			raw_text.into(),
			dest,
			known_roots,
			generation,
			opts,
			limit,
			remote,
		)
	}

	/// [`Self::build_at`] over a payload that is already allocated, which
	/// the plan then shares: a retarget plans the same text again without a
	/// second copy of it.
	fn build_shared(
		raw: Arc<str>,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
		remote: Option<RemotePaste>,
	) -> Result<Self, Msg> {
		let (plan, failed) = Self::plan_shared(
			raw,
			dest,
			known_roots,
			generation,
			opts,
			limit,
			remote,
		)?;
		// A refusal that carries the engine's suggestion lands as the panel
		// it was opened on, like a retarget's (`retarget_with`): not
		// executable, with the error, and [Adjust paths] recovers it. One
		// without a suggestion leaves nothing to keep.
		if plan.restore_hint().is_some() {
			return Ok(plan);
		}
		if let Some(err) = failed {
			return Err(err);
		}
		// A replay has no file plan: its preview is the plan.
		if !plan.whole_commit
			&& plan.prefix_choices.is_empty()
			&& plan.import_plan.is_none()
		{
			return Err(plan
				.error
				.unwrap_or_else(|| Msg::new("paste_err_nothing", [])));
		}
		Ok(plan)
	}

	/// [`Self::build_shared`] without judging what the engine answered: the
	/// plan comes back whatever it said, a refusal as the plan's `error`
	/// beside the restore-base suggestion the engine made while refusing it
	/// (what [Adjust paths] recovers). The second value is the error of a
	/// plan that could not be built at all (the destination did not resolve,
	/// the worker was unreachable). Only what leaves no plan to keep (the
	/// memory budget, a payload that is none, a replay) is an `Err`.
	fn plan_shared(
		raw: Arc<str>,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
		remote: Option<RemotePaste>,
	) -> Result<(Self, Option<Msg>), Msg> {
		check_budget(raw.len(), limit)?;
		let raw_text: &str = &raw;
		if commits::is_commit_payload(raw_text) {
			return Self::build_commit(
				raw.clone(),
				dest,
				generation,
				opts,
				limit,
				remote,
			)
			.map(|plan| (plan, None));
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
		// A remote workspace's folders are tree paths, not local ones.
		let candidates = if remote.is_some() {
			roots.clone()
		} else {
			canonical_dirs(&roots)
		};
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
			commit_prepared: Default::default(),
			whole_commit: false,
			collapsed_commits: BTreeSet::new(),
			collapsed_dirs: BTreeSet::new(),
			cursor_dir: None,
			fold_seeded: false,
			prefix_choices: Vec::new(),
			seen_roots: roots_digest(known_roots),
			restore_base: None,
			adjust_paths: false,
			plan: empty_plan(),
			items: Vec::new(),
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw.clone(),
			remote,
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
		match plan.rebuild_with_limit(opts, limit) {
			Ok(()) => Ok((plan, None)),
			Err(err) if err.key == preview_budget_error().key => Err(err),
			// `rebuild_with_limit` left the error on the plan.
			Err(err) => Ok((plan, Some(err))),
		}
	}

	fn build_commit(
		raw: Arc<str>,
		dest: &Path,
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
		remote: Option<RemotePaste>,
	) -> Result<Self, Msg> {
		let raw_text: &str = &raw;
		let payload = commits::parse_commit_payload(raw_text).map_err(|e| {
			Msg::with_msg_arg(
				"paste_err_plan",
				[String::new()],
				0,
				reason_msg(&e.reason()),
			)
		})?;
		// The write scope is the folder the user pasted into: the worker
		// holds the replay to it the same way.
		let target = paste_target(remote.as_ref(), dest)?;
		let (preview, prepared) = target
			.plan_commits(raw_text, payload, opts)
			.map_err(|e| destination_msg(remote.as_ref(), dest, e))?;
		let preview = Arc::new(preview);
		// Rows name the worker's repository in a remote workspace; the plan
		// keeps the tree path the user pasted into.
		let destination = match &remote {
			None => preview.destination().to_path_buf(),
			Some(_) => dest.to_path_buf(),
		};
		let dest = match &remote {
			None => dest,
			Some(_) => preview.destination(),
		};
		let mut plan = Self {
			destination,
			import_plan: None,
			commit_preview: Some(preview.clone()),
			commit_prepared: prepared,
			whole_commit: true,
			collapsed_commits: BTreeSet::new(),
			collapsed_dirs: BTreeSet::new(),
			cursor_dir: None,
			fold_seeded: false,
			prefix_choices: Vec::new(),
			seen_roots: 0,
			restore_base: None,
			adjust_paths: false,
			plan: empty_plan(),
			items: Vec::new(),
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw.clone(),
			remote,
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
			// Rows follow the Log's natural path order (g1, g2, g10), not replay
			// order; op_index keeps the replay index (#196). A rename's two rows
			// stay together under the new path.
			let n = commit.files.len().min(record.files.len());
			let mut order: Vec<usize> = (0..n).collect();
			order.sort_by(|&a, &b| {
				crate::ui::natural_cmp(
					&commit.files[a].path,
					&commit.files[b].path,
				)
			});
			for f_idx in order {
				let (file, src) = (&commit.files[f_idx], &record.files[f_idx]);
				let op = if let Some(conflict) = file.layout_conflict {
					PlannedOp::CommitRefused(conflict)
				} else {
					match file.action {
						ReplayAction::Delete => PlannedOp::Delete,
						ReplayAction::Write if file.overwrites_existing() => {
							PlannedOp::Overwrite
						}
						ReplayAction::Write => PlannedOp::Create,
						ReplayAction::Skip => PlannedOp::Skip(skip_cause(file)),
					}
				};
				let body = src.content.as_deref().unwrap_or_default();
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
						op_index: f_idx,
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
						String::new(),
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
					body.to_string(),
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
		// A remote folder is checked by its worker when the plan is made.
		let path = if self.remote.is_some() {
			dest.to_path_buf()
		} else {
			let id = CanonicalRootId::new(dest)
				.map_err(|e| destination_error(dest, &e))?;
			if !id.path().is_dir() {
				return Err(Msg::new(
					"paste_err_destination_not_dir",
					[id.path().display().to_string()],
				));
			}
			id.path().to_path_buf()
		};
		let choice = self
			.prefix_choices
			.iter_mut()
			.find(|c| c.prefix == prefix)
			.ok_or_else(|| {
				Msg::new("mapping_unknown_prefix", [prefix.to_string()])
			})?;
		if !choice.candidates.contains(&path) {
			return Err(Msg::new(
				"mapping_unknown_dest",
				[path.display().to_string()],
			));
		}
		choice.keep_relative = false;
		choice.destination = Some(path);
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

	/// The restore-base suggestion the panel offers: only for a payload
	/// without prefix rows (a `clipcode-root` export) and not for a commit
	/// replay. With prefix rows the user routes each prefix instead (D4).
	pub fn restore_hint(&self) -> Option<&RestoreBaseSuggestion> {
		if self.whole_commit || !self.prefix_choices.is_empty() {
			return None;
		}
		self.restore_base.as_ref()
	}

	/// Records the hint's choice only, like [`Self::choose_keep_relative`].
	pub fn choose_adjust_paths(&mut self, on: bool) -> Result<(), Msg> {
		if self.restore_hint().is_none() {
			return Err(Msg::new("paste_adjust_unavailable", []));
		}
		self.adjust_paths = on;
		Ok(())
	}

	/// Plans with (`true`) or without the suggestion. Blocking, for tests.
	pub fn set_adjust_paths(&mut self, on: bool) -> Result<(), Msg> {
		self.choose_adjust_paths(on)?;
		self.rebuild_file_plan_with(&RunOptions::default())
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

	/// Turns this plan into the empty shell of a retarget to `dest`: the
	/// destination moves and nothing planned for the old one can be applied
	/// while the new plan is built. Unlike [`Self::clear_file_plan`] it also
	/// drops a whole-commit replay (which has no mapping to rebuild from),
	/// and the prefix choices, which are the new destination's to ask again.
	/// The tree's folds and folder cursor name rows under the old folder's
	/// roots, and the restore-base hint is what the old folder's engine
	/// suggested: both start over with the new folder, as on a new paste,
	/// and [Adjust paths] is off again until the user asks.
	pub(crate) fn disarm_for(&mut self, dest: PathBuf) {
		self.destination = dest;
		self.items = Vec::new();
		self.selected_item_idx = 0;
		self.import_plan = None;
		self.plan = empty_plan();
		self.commit_preview = None;
		self.commit_prepared = Default::default();
		self.collapsed_commits.clear();
		self.collapsed_dirs.clear();
		self.cursor_dir = None;
		self.fold_seeded = false;
		self.restore_base = None;
		self.adjust_paths = false;
		self.prefix_choices = Vec::new();
		self.error = None;
	}

	/// This disarmed shell's payload planned into `dest`. A destination the
	/// engine cannot use (it does not exist, the worker refused it, a commit
	/// replay has no repo there) or a plan it refuses is not a failure of
	/// the job: the shell lands with the engine's own words, and with the
	/// restore-base suggestion it made, so the panel stays on the folder the
	/// user picked and can recover it. Only the memory budget refuses the
	/// landing.
	pub(crate) fn retarget_with(
		mut self,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: crate::gen::Stamp<crate::gen::RepoLoadLane>,
		opts: &RunOptions,
		limit: usize,
	) -> Result<Self, Msg> {
		let planned = Self::plan_shared(
			self.raw_payload.clone(),
			dest,
			known_roots,
			generation,
			opts,
			limit,
			self.remote.clone(),
		);
		match planned {
			// A refused plan is the shell the panel stays on: it keeps the
			// engine's words and the suggestion that recovers it.
			Ok((plan, _)) => Ok(plan),
			Err(err) if err.key == preview_budget_error().key => Err(err),
			Err(err) => {
				self.error = Some(err);
				Ok(self)
			}
		}
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
		// `restore_base` is left as it was on the early returns below: they
		// run before any engine hears the payload, so the last suggestion
		// still describes it and keeps [Use as-is] reachable.
		if !self.mapping_ready() {
			self.error = Some(Msg::new("mapping_required", []));
			return Ok(());
		}
		let (target, route) = self.target_route()?;
		let mut heard: Option<RestoreBaseSuggestion> = None;
		let planned = {
			let mut note = |s: &RestoreBaseSuggestion| heard = Some(s.clone());
			// Only a payload without prefix rows (a `clipcode-root` export)
			// shows the hint; with rows the user routes each prefix (D4).
			let note: Option<&mut dyn FnMut(&RestoreBaseSuggestion)> =
				if self.prefix_choices.is_empty() {
					Some(&mut note)
				} else {
					None
				};
			target.plan_files(&self.raw_payload, &route, opts, note)
		};
		// A refusal that carried no suggestion (the destination did not
		// resolve, the worker was unreachable) keeps the last one: the hint
		// row must stay so [Use as-is] can undo an adjustment the engine
		// refused. An engine that refused the plan, locally or on a worker,
		// said what it suggested first, so its refusal updates it.
		if planned.is_ok() || heard.is_some() {
			self.restore_base = heard;
		}
		let file_plan = match planned {
			Ok(plan) => plan,
			// No engine answered: the destination did not resolve or the
			// worker could not be reached.
			Err(
				err @ (PasteError::Destination { .. }
				| PasteError::Remote(
					RemoteError::Io(_)
					| RemoteError::Start { .. }
					| RemoteError::PatchTooLarge { .. }
					| RemoteError::Protocol(_)
					| RemoteError::Cancelled
					| RemoteError::TimedOut
					| RemoteError::Stalled { .. }
					| RemoteError::WorkerTooOld { .. },
				)
				| PasteError::OutcomeUnknown(_)
				| PasteError::NoReplayResult),
			) => {
				return Err(destination_msg(
					self.remote.as_ref(),
					&self.destination,
					err,
				));
			}
			// An engine refused the plan, locally or on the worker.
			Err(err) => {
				if crate::e2e_on() && err.is_collision() {
					app_log!(
						"[APP:PASTE_PLAN_REFUSED: reason=target_collision]"
					);
				}
				self.error = Some(Msg::with_msg_arg(
					"paste_err_plan",
					[String::new()],
					0,
					reason_msg(&err.reason()),
				));
				return Ok(());
			}
		};
		let base = self
			.retained_bytes()
			.saturating_add(size_of::<FilePastePlan>() + ARC_ALLOWANCE)
			.saturating_add(file_plan.retained_heap_bytes())
			.saturating_add(file_plan.restore_plan().retained_heap_bytes());
		// The existing empty Arc<RestorePlan> charge covers its replacement.
		// Check before making the second full restore-plan copy.
		check_budget(base, limit)?;
		let planned = file_plan.restore_plan().clone();
		if planned.create_operations.is_empty()
			&& planned.delete_operations.is_empty()
			&& planned.skipped_operations.is_empty()
		{
			self.error = Some(Msg::new("paste_err_nothing", []));
			return Ok(());
		}
		let items = self.fill_items(&file_plan, &planned, base, limit)?;
		self.plan = Arc::new(planned);
		self.import_plan = Some(Arc::new(file_plan));
		self.items = items;
		self.error = None;
		self.seed_folds();
		Ok(())
	}

	/// This plan's target and the chosen prefix folders, spelled for it.
	fn target_route(&self) -> Result<(PasteTarget, PasteRoute), Msg> {
		let target = paste_target(self.remote.as_ref(), &self.destination)?;
		let mut route = PasteRoute {
			adjust_paths: self.adjust_paths,
			..Default::default()
		};
		for choice in &self.prefix_choices {
			if choice.keep_relative {
				continue;
			}
			let Some(folder) = &choice.destination else {
				continue;
			};
			let folder = match &self.remote {
				None => folder.clone(),
				Some(r) => PathBuf::from(r.rel(folder)?),
			};
			route.prefixes.push((choice.prefix.clone(), folder));
		}
		Ok((target, route))
	}

	fn fill_items(
		&self,
		file_plan: &FilePastePlan,
		plan: &RestorePlan,
		base: usize,
		limit: usize,
	) -> Result<Vec<PasteItem>, Msg> {
		let mut items = Vec::new();
		let mut items_heap = 0usize;
		// The roots as the plan's rows spell them. The engine keeps the
		// destination as it was given (through a symlink, say), while a
		// row's own root is the canonical folder; a worker's are its own.
		let roots: Vec<PathBuf> = file_plan
			.roots()
			.iter()
			.map(|r| match &self.remote {
				None => dunce::canonicalize(r).unwrap_or_else(|_| r.clone()),
				Some(_) => r.clone(),
			})
			.collect();
		// The root a row without its own is shown under (a skipped row has
		// no place at all): the first one, where unprefixed paths land.
		let primary = roots
			.first()
			.cloned()
			.unwrap_or_else(|| self.destination.clone());
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
			// The root the delete's path is relative to, as a create's
			// `root_path` is: the tree places a row at root + path, and
			// Apply deletes `absolute_path`. The innermost root above the
			// file is only a fallback: a nested root can lie between.
			let dest_root =
				plan_root_of(&roots, &op.absolute_path, &op.relative_path)
					.or_else(|| innermost_root(&roots, &op.absolute_path))
					.cloned()
					.unwrap_or_else(|| primary.clone());
			let item = PasteItem {
				path: op.relative_path.clone(),
				dest_root_name: root_name(&dest_root),
				dest_path: op.absolute_path.clone(),
				// A worker's paths are not this machine's: its snapshot answers.
				dest_exists: file_plan.delete_target_exists(op_idx),
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
		for (op_idx, op) in plan.skipped_operations.iter().enumerate() {
			let reason = if op.detail
				== Some(snip_core::restore::SkipDetail::ProtectedGitMetadata)
			{
				SkipCause::ProtectedGitMetadata
			} else {
				SkipCause::Restore(op.reason)
			};
			let item = PasteItem {
				path: op.raw_path.clone(),
				dest_root: primary.clone(),
				dest_root_name: root_name(&primary),
				// There is no admitted destination for a skipped operation.
				dest_path: self.destination.clone(),
				dest_exists: false,
				op: PlannedOp::Skip(reason),
				overwrite_allowed: false,
				selected: false,
				content: Arc::from(""),
				bytes: 0,
				lines: 0,
				op_index: op_idx,
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
			if item.overwritable() {
				item.overwrite_allowed = !item.overwrite_allowed;
			}
		}
	}

	pub fn all_selected(&self) -> bool {
		self.items.iter().all(|i| i.selected)
	}

	pub fn toggle_selected(&mut self, idx: usize) {
		if let Some(item) = self.items.get_mut(idx) {
			if !item.planned_skip() {
				item.selected = !item.selected;
			}
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
		let target = self.step_target(false);
		self.put_cursor(target);
	}

	#[cfg(test)]
	pub fn select_next(&mut self) {
		let target = self.step_target(true);
		self.put_cursor(target);
	}

	#[cfg(test)]
	fn put_cursor(&mut self, cursor: PasteCursor) {
		match cursor {
			PasteCursor::Item(ix) => {
				self.selected_item_idx = ix;
				self.cursor_dir = None;
			}
			PasteCursor::Dir(key) => self.cursor_dir = Some(key),
		}
	}

	/// A folder row as the logs and drivers name it: `<ord>:<rel>`, the
	/// root's position on screen (a replay's: the commit's index) and the
	/// folder's path under it.
	pub fn dir_label(&self, key: &DirKey) -> String {
		// A replay has one root; its folders are told apart by commit.
		let ord = key.commit.unwrap_or_else(|| {
			self.root_groups()
				.iter()
				.position(|g| g.root == key.root)
				.unwrap_or(0)
		});
		format!("{ord}:{}", key.path)
	}

	/// Where the cursor is: on a folder row, or on an item.
	pub fn cursor(&self) -> PasteCursor {
		self.cursor_dir
			.clone()
			.map_or(PasteCursor::Item(self.selected_item_idx), PasteCursor::Dir)
	}

	/// The rows the cursor can stand on, in screen order, with their tree
	/// depth and, for a folder, whether it is folded. A commit replay has
	/// only its file rows (its headers and folders fold, they are not
	/// selected).
	pub fn cursor_rows(&self) -> Vec<(PasteCursor, usize, Option<bool>)> {
		let nodes = if self.whole_commit {
			self.commit_rows()
		} else {
			self.tree_rows()
		};
		let by_commit = self.whole_commit;
		nodes
			.into_iter()
			.filter_map(|n| match n {
				PasteNode::Root { key, collapsed, .. } => {
					Some((PasteCursor::Dir(key), 0, Some(collapsed)))
				}
				// A commit's folders fold like its headers do: they are
				// clicked, the cursor does not stand on them.
				PasteNode::Dir { .. } if by_commit => None,
				PasteNode::Dir {
					key,
					depth,
					collapsed,
					..
				} => Some((PasteCursor::Dir(key), depth, Some(collapsed))),
				PasteNode::File(ix, depth) => {
					Some((PasteCursor::Item(ix), depth, None))
				}
				PasteNode::Commit(_) => None,
			})
			.collect()
	}

	/// The row Up / Down moves to. Changes nothing.
	pub fn step_target(&self, forward: bool) -> PasteCursor {
		let rows = self.cursor_rows();
		let current = self.cursor();
		let Some(pos) = rows.iter().position(|(c, ..)| *c == current) else {
			// The cursor row is folded away: re-enter at the near end.
			let edge = if forward { rows.first() } else { rows.last() };
			return edge.map_or(current, |(c, ..)| c.clone());
		};
		let next = if forward {
			pos.checked_add(1)
		} else {
			pos.checked_sub(1)
		};
		next.and_then(|n| rows.get(n))
			.map_or(current, |(c, ..)| c.clone())
	}

	/// Item indices in the order the rows show: by destination root, then
	/// the directory tree (folders before files). A commit replay lists its
	/// visible rows in natural path order like the Log instead, exactly as
	/// [`Self::commit_rows`] shows them. Rows folded away are left out.
	pub fn display_order(&self) -> Vec<usize> {
		let nodes = if self.whole_commit {
			self.commit_rows()
		} else {
			self.tree_rows()
		};
		nodes
			.into_iter()
			.filter_map(|n| match n {
				PasteNode::File(ix, _) => Some(ix),
				_ => None,
			})
			.collect()
	}

	/// The items of each destination root, roots in natural name order.
	/// Rows skipped when planned carry their raw clipboard path and no real
	/// place, so they are kept apart from the rows that have one.
	fn root_groups(&self) -> Vec<RootGroup> {
		use crate::ui::natural_cmp;
		let mut groups: Vec<RootGroup> = Vec::new();
		let mut order: Vec<usize> = (0..self.items.len()).collect();
		order.sort_by(|&a, &b| {
			let (x, y) = (&self.items[a], &self.items[b]);
			natural_cmp(&x.dest_root_name, &y.dest_root_name)
				.then_with(|| x.dest_root.cmp(&y.dest_root))
				.then_with(|| a.cmp(&b))
		});
		for ix in order {
			let item = &self.items[ix];
			if groups.last().is_none_or(|g| g.root != item.dest_root) {
				groups.push(RootGroup {
					root: item.dest_root.clone(),
					label: item.dest_root_name.clone(),
					all: Vec::new(),
					placed: Vec::new(),
					skipped: Vec::new(),
				});
			}
			let group = groups.last_mut().expect("a group was just pushed");
			group.all.push(ix);
			if item.planned_skip() {
				group.skipped.push(ix);
			} else {
				group.placed.push(ix);
			}
		}
		for group in &mut groups {
			group.skipped.sort_by(|&a, &b| {
				natural_cmp(&self.items[a].path, &self.items[b].path)
			});
		}
		groups
	}

	fn summary(&self, members: &[usize]) -> DirSummary {
		let mut sum = DirSummary::default();
		for &ix in members {
			let item = &self.items[ix];
			sum.files += 1;
			if self.whole_commit {
				// A replay is written whole or not at all: nothing under a
				// folder can be toggled, and "included" is what Apply writes.
				sum.included += usize::from(
					item.selected && !item.action().is_not_written(),
				);
			} else if !item.planned_skip() {
				sum.toggleable += 1;
				sum.included += usize::from(item.selected);
			}
		}
		sum
	}

	/// The change tree IntelliJ's Apply Patch dialog shows: one root node per
	/// destination root, one node per directory (a chain of single-child
	/// directories is one), then its files. Folders come before files.
	/// Folded folders keep their node and drop their rows.
	pub fn tree_rows(&self) -> Vec<PasteNode> {
		let mut rows = Vec::new();
		for (ord, group) in self.root_groups().into_iter().enumerate() {
			let key = DirKey {
				commit: None,
				root: group.root.clone(),
				path: String::new(),
			};
			let collapsed = self.collapsed_dirs.contains(&key);
			rows.push(PasteNode::Root {
				key,
				ord,
				label: group.label.clone(),
				sum: self.summary(&group.all),
				collapsed,
			});
			if collapsed {
				continue;
			}
			let mut placed = group.placed;
			let mut sink = PasteSink {
				plan: self,
				root: &group.root,
				ord,
				rows: &mut rows,
			};
			DirTree { files: &self.items }.push(&mut sink, &mut placed, 0, 1);
			rows.extend(
				group.skipped.into_iter().map(|ix| PasteNode::File(ix, 1)),
			);
		}
		rows
	}

	/// Every folder row of the fully open tree: (root, path, depth).
	fn all_dirs(&self) -> Vec<(RootKey, String, usize)> {
		struct All(Vec<(RootKey, String, usize)>, RootKey);
		impl TreeSink for All {
			fn dir(
				&mut self,
				path: &str,
				_: &str,
				_: &[usize],
				depth: usize,
			) -> bool {
				self.0.push((self.1.clone(), path.to_string(), depth));
				true
			}

			fn file(&mut self, _: usize, _: usize) {}
		}
		let mut found = Vec::new();
		for group in self.root_groups() {
			let mut all = All(Vec::new(), group.root);
			let mut placed = group.placed;
			DirTree { files: &self.items }.push(&mut all, &mut placed, 0, 1);
			found.extend(all.0);
		}
		found
	}

	/// The outermost folded folder that hides item `ix`, if any.
	fn hiding_dir(&self, ix: usize) -> Option<DirKey> {
		let item = self.items.get(ix)?;
		self.collapsed_dirs
			.iter()
			.filter(|k| {
				k.root == item.dest_root
					&& (k.path.is_empty()
						|| (!item.planned_skip()
							&& path_under(&item.path, &k.path)))
			})
			.min_by_key(|k| k.path.len())
			.cloned()
	}

	/// Folds the first level of a big preview, once, when its rows first
	/// arrive (a mapping can make that a later remap, not the first build).
	/// A cursor the fold hides moves onto the folder that hides it.
	fn seed_folds(&mut self) {
		if self.fold_seeded || self.items.is_empty() {
			return;
		}
		self.fold_seeded = true;
		if self.items.len() <= FOLD_FILES_OVER {
			return;
		}
		for (root, path, depth) in self.all_dirs() {
			if depth == 1 {
				self.collapsed_dirs.insert(DirKey {
					commit: None,
					root,
					path,
				});
			}
		}
		if let Some(key) = self.hiding_dir(self.selected_item_idx) {
			self.cursor_dir = Some(key);
		}
	}

	/// Drops state that no longer names a row, after a remap or a rebuild:
	/// folds of folders that are gone, a folder cursor that is gone or
	/// hidden. A cursor left on a hidden item moves onto its folder.
	/// Only ever makes the state smaller.
	pub fn normalize_cursor(&mut self) {
		if self.whole_commit {
			return;
		}
		let mut exists: BTreeSet<DirKey> = self
			.all_dirs()
			.into_iter()
			.map(|(root, path, _)| DirKey {
				commit: None,
				root,
				path,
			})
			.collect();
		exists.extend(self.root_groups().into_iter().map(|g| DirKey {
			commit: None,
			root: g.root,
			path: String::new(),
		}));
		self.collapsed_dirs.retain(|k| exists.contains(k));
		let visible = |plan: &Self| {
			let cursor = plan.cursor();
			plan.cursor_rows().iter().any(|(c, ..)| *c == cursor)
		};
		if self.cursor_dir.is_some() && !visible(self) {
			self.cursor_dir = None;
		}
		if self.cursor_dir.is_none() && !visible(self) {
			self.cursor_dir = self.hiding_dir(self.selected_item_idx);
		}
	}

	/// Folds or unfolds one folder's rows.
	///
	/// Does not move the cursor: the session does, so it can admit the new
	/// state first. When the fold hides the cursor, returns the folder to
	/// move onto, so Space and the detail pane never act on a row the user
	/// cannot see.
	pub fn toggle_dir_collapsed(
		&mut self,
		key: &DirKey,
	) -> Option<PasteCursor> {
		if self.whole_commit {
			// The cursor never stands on a replay's folder: it goes to the
			// nearest row still shown, as when a header folds.
			let before = self.display_order();
			if !self.collapsed_dirs.remove(key) {
				self.collapsed_dirs.insert(key.clone());
			}
			return self.row_left_hidden(&before).map(PasteCursor::Item);
		}
		if self.collapsed_dirs.remove(key) {
			return None;
		}
		self.collapsed_dirs.insert(key.clone());
		let cursor = self.cursor();
		let visible = self.cursor_rows().iter().any(|(c, ..)| *c == cursor);
		(!visible).then(|| PasteCursor::Dir(key.clone()))
	}

	/// What Left does at the cursor: fold an open folder, else go up to the
	/// folder that holds this row.
	pub fn nav_left(&self) -> TreeStep {
		if self.whole_commit {
			return TreeStep::Nothing;
		}
		let rows = self.cursor_rows();
		let cursor = self.cursor();
		let Some(pos) = rows.iter().position(|(c, ..)| *c == cursor) else {
			return TreeStep::Nothing;
		};
		let (here, depth, collapsed) = &rows[pos];
		if let (PasteCursor::Dir(key), Some(false)) = (here, collapsed) {
			return TreeStep::Fold(key.clone());
		}
		rows[..pos]
			.iter()
			.rev()
			.find(|(_, d, _)| d < depth)
			.map_or(TreeStep::Nothing, |(c, ..)| TreeStep::Move(c.clone()))
	}

	/// What Right does at the cursor: open a folded folder, else go to its
	/// first row.
	pub fn nav_right(&self) -> TreeStep {
		if self.whole_commit {
			return TreeStep::Nothing;
		}
		let rows = self.cursor_rows();
		let cursor = self.cursor();
		let Some(pos) = rows.iter().position(|(c, ..)| *c == cursor) else {
			return TreeStep::Nothing;
		};
		let (here, depth, collapsed) = &rows[pos];
		match (here, collapsed) {
			(PasteCursor::Dir(key), Some(true)) => {
				TreeStep::Unfold(key.clone())
			}
			(PasteCursor::Dir(_), Some(false)) => rows
				.get(pos + 1)
				.filter(|(_, d, _)| d > depth)
				.map_or(TreeStep::Nothing, |(c, ..)| TreeStep::Move(c.clone())),
			_ => TreeStep::Nothing,
		}
	}

	/// Items a folder's include box covers: the rows under it that can be
	/// toggled. A sibling that only shares the name's start is not under it.
	fn dir_members<'a>(
		&'a self,
		key: &'a DirKey,
	) -> impl Iterator<Item = usize> + 'a {
		self.items.iter().enumerate().filter_map(move |(ix, item)| {
			(item.dest_root == key.root
				&& !item.planned_skip()
				&& (key.path.is_empty() || path_under(&item.path, &key.path)))
			.then_some(ix)
		})
	}

	/// The include box of a folder: everything under it on unless it is all
	/// on already, then everything off. `None` when nothing can be toggled
	/// (a commit replay writes whole or not at all).
	pub fn set_dir_selected(&mut self, key: &DirKey) -> Option<bool> {
		if self.whole_commit {
			return None;
		}
		let members: Vec<usize> = self.dir_members(key).collect();
		if members.is_empty() {
			return None;
		}
		let on = !members.iter().all(|&ix| self.items[ix].selected);
		for ix in members {
			self.items[ix].selected = on;
		}
		Some(on)
	}

	/// The folder the detail pane describes: the cursor's folder, while its
	/// row is shown. A remap clears the rows but keeps the cursor for the
	/// next landing, and a folder with no rows has nothing to describe.
	pub fn detail_dir(&self) -> Option<&DirKey> {
		let key = self.cursor_dir.as_ref()?;
		(!self.items.is_empty() && self.has_dir_row(key)).then_some(key)
	}

	/// Whether a folder or root row with this key is shown now.
	pub fn has_dir_row(&self, key: &DirKey) -> bool {
		let nodes = if self.whole_commit {
			self.commit_rows()
		} else {
			self.tree_rows()
		};
		nodes.iter().any(|n| {
			matches!(n, PasteNode::Root { key: k, .. }
				| PasteNode::Dir { key: k, .. } if k == key)
		})
	}

	/// Where item `ix` stands in the tree. A commit's rename is a deletion of
	/// the old path next to a write of the new one, and the pair stays
	/// together under the new path (#196), where the user will look for it.
	pub fn place_path(&self, ix: usize) -> &str {
		let item = &self.items[ix];
		if item.is_rename_source() {
			if let Some(next) = self.items.get(ix + 1) {
				if next.commit == item.commit && next.op_index == item.op_index
				{
					return &next.path;
				}
			}
		}
		&item.path
	}

	/// The commit replay as the user will see it: each commit's header, in
	/// replay order, followed by its rows as the same directory tree a file
	/// paste shows (folders first, natural path order). An empty commit is a
	/// header with no rows; a collapsed commit keeps only its header, and a
	/// collapsed folder only its own row.
	pub fn commit_rows(&self) -> Vec<PasteNode> {
		let Some(preview) = &self.commit_preview else {
			return Vec::new();
		};
		let mut rows = Vec::with_capacity(self.items.len());
		let mut start = 0;
		for c in 0..preview.plan().commits.len() {
			rows.push(PasteNode::Commit(c));
			let len = self.items[start..]
				.iter()
				.take_while(|i| i.commit == Some(c))
				.count();
			if len > 0 && !self.collapsed_commits.contains(&c) {
				let places: Vec<Placed> = (start..start + len)
					.map(|ix| Placed(self.place_path(ix)))
					.collect();
				let mut entries: Vec<usize> = (0..len).collect();
				let mut sink = CommitSink {
					plan: self,
					commit: c,
					base: start,
					rows: &mut rows,
				};
				DirTree { files: &places }.push(&mut sink, &mut entries, 0, 1);
			}
			start += len;
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
		self.row_left_hidden(&before)
	}

	/// After a fold of a replay's rows: when the selected row went out of
	/// view, the row to select instead (the next one shown, else the
	/// previous). `before` is the order shown before the fold.
	fn row_left_hidden(&self, before: &[usize]) -> Option<usize> {
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
		tally(self.items.iter())
	}

	/// The counts of the rows under a folder, skipped rows included at the
	/// root (they show there).
	pub fn counts_under(&self, key: &DirKey) -> PasteCounts {
		tally(self.items.iter().enumerate().filter_map(|(ix, it)| {
			(it.dest_root == key.root
				&& it.commit == key.commit
				&& (key.path.is_empty()
					|| (!it.planned_skip()
						&& path_under(self.place_path(ix), &key.path))))
			.then_some(it)
		}))
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
		let target = paste_target(self.remote.as_ref(), &self.destination)?;
		if let Some(preview) = &self.commit_preview {
			target
				.check_commits(
					&self.raw_payload,
					preview,
					&self.commit_prepared,
				)
				.map_err(|e| {
					commit_msg(
						self.remote.as_ref(),
						&self.destination,
						e,
						false,
					)
				})?;
		}
		if let Some(plan) = &self.import_plan {
			target.check_files(plan).map_err(|e| {
				file_apply_msg(self.remote.as_ref(), &self.destination, e)
			})?;
		}
		Ok(())
	}

	pub fn execute(&self) -> Result<PasteApplyResult, ApplyRefusal> {
		if let Some(preview) = &self.commit_preview {
			return self.execute_commit(preview);
		}
		if !self.mapping_ready() {
			return Err(ApplyRefusal::app(
				Msg::new("mapping_required", []),
				ApplyToken::MappingRequired,
			));
		}

		let Some(ref import_plan) = self.import_plan else {
			// An invariant break: Apply without a plan.
			return Err(ApplyRefusal::app(
				Msg::with_msg_arg(
					"paste_err_plan",
					[String::new()],
					0,
					reason_msg(&snip_core::reason::Reason::Internal {
						detail: snip_core::reason::Datum::default(),
						condition: None,
					}),
				),
				ApplyToken::PasteErrPlan,
			));
		};

		let mut unchecked_creates = BTreeSet::new();
		let mut unchecked_deletes = BTreeSet::new();
		let mut skipped_existing = 0;

		for item in &self.items {
			if item.planned_skip() {
				continue;
			}
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
			..Default::default()
		};

		// Run confirmed plan directly through shared transfer engine with freshness check
		let (target, route) = self.target_route().map_err(|m| {
			ApplyRefusal::app(m, ApplyToken::PasteErrDestination)
		})?;
		let mut result = target
			.apply_files(&self.raw_payload, &route, import_plan, &selection)
			.map_err(|e| {
				file_apply_refusal(self.remote.as_ref(), &self.destination, e)
			})?;
		result.skipped_existing_count += skipped_existing;
		Ok(PasteApplyResult {
			files: result,
			created_commits: Vec::new(),
		})
	}

	fn execute_commit(
		&self,
		preview: &CommitReplayPreview,
	) -> Result<PasteApplyResult, ApplyRefusal> {
		if self.items.iter().any(|item| !item.selected) {
			return Err(ApplyRefusal::app(
				Msg::new("commit_subset_rejected", []),
				ApplyToken::CommitSubsetRejected,
			));
		}
		let target = paste_target(self.remote.as_ref(), &self.destination)
			.map_err(|m| {
				ApplyRefusal::app(m, ApplyToken::PasteErrDestination)
			})?;
		// Core overwrites (spec 4.3); asking first is this session's rule. Stale still wins over the prompt, as before.
		if self.overwrite_missing() {
			target
				.check_commits(
					&self.raw_payload,
					preview,
					&self.commit_prepared,
				)
				.map_err(|e| {
					commit_refusal(
						self.remote.as_ref(),
						&self.destination,
						e,
						false,
					)
				})?;
			return Err(ApplyRefusal::app(
				Msg::new("commit_overwrite_required", []),
				ApplyToken::CommitOverwriteRequired,
			));
		}
		let replay_res = target
			.apply_commits(&self.raw_payload, preview, &self.commit_prepared)
			.map_err(|e| {
				commit_refusal(self.remote.as_ref(), &self.destination, e, true)
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
				let (cause_arg, cause) = match fail.layout_conflict {
					// A layout conflict words itself by its key.
					Some(conflict) => {
						key_args.push(2);
						(layout_conflict_cause_key(conflict).to_string(), None)
					}
					// Any other refusal by its reason. A worker that sent
					// none (an old one) is worded by the generic I/O cause.
					None => (
						String::new(),
						Some(reason_msg(&replay_failure_reason(&fail))),
					),
				};
				let msg = Msg::with_key_args(
					"commit_replay_refused",
					[
						(fail.index + 1).to_string(),
						subject_arg.to_string(),
						cause_arg,
					],
					key_args,
				);
				return Err(ApplyRefusal::app(
					match cause {
						Some(cause) => msg.nest(2, cause),
						None => msg,
					},
					ApplyToken::CommitReplayRefused,
				));
			}
			let created = short_shas(&replay_res.created, usize::MAX);
			return Err(match fail.layout_conflict {
				Some(conflict) => {
					let subject =
						fail.message.lines().next().unwrap_or("").trim();
					let path = fail.conflict_path.as_deref().unwrap_or(subject);
					ApplyRefusal::app(
						Msg::with_key_arg(
							"commit_replay_partial_refused",
							[
								created,
								(fail.index + 1).to_string(),
								path.to_string(),
								layout_conflict_cause_key(conflict).to_string(),
							],
							3,
						),
						ApplyToken::CommitReplayPartialRefused,
					)
				}
				None => ApplyRefusal::app(
					Msg::with_msg_arg(
						"commit_replay_partial",
						[created, String::new()],
						1,
						reason_msg(&replay_failure_reason(&fail)),
					),
					ApplyToken::CommitReplayPartial,
				),
			});
		}
		Ok(PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: replay_res.created.len(),
				overwritten_count: 0,
				deleted_count: 0,
				skipped_existing_count: 0,
				planned_skipped_count: 0,
				errors: Vec::new(),
				failures: Vec::new(),
				..Default::default()
			},
			created_commits: replay_res.created,
		})
	}

	/// Makes the plan forget the repo set it last saw (a breaker of R4).
	#[cfg(test)]
	pub(crate) fn forget_roots_for_tests(&mut self) {
		self.seen_roots = !self.seen_roots;
	}

	/// Whether the cursor stands on a row that is drawn (P5 of
	/// `invariants`): the one question `normalize_cursor` settles.
	pub(crate) fn cursor_visible(&self) -> bool {
		let cursor = self.cursor();
		self.cursor_rows().iter().any(|(c, ..)| *c == cursor)
	}

	/// How many folded folders no longer name a row (P6 of `invariants`).
	/// The set of folders that exist is the one `normalize_cursor` keeps.
	pub(crate) fn stale_folds(&self) -> usize {
		if self.whole_commit {
			return 0;
		}
		let mut exists: BTreeSet<DirKey> = self
			.all_dirs()
			.into_iter()
			.map(|(root, path, _)| DirKey {
				commit: None,
				root,
				path,
			})
			.collect();
		exists.extend(self.root_groups().into_iter().map(|g| DirKey {
			commit: None,
			root: g.root,
			path: String::new(),
		}));
		self.collapsed_dirs
			.iter()
			.filter(|k| !exists.contains(*k))
			.count()
	}
}

#[cfg(test)]
pub(crate) mod zh_leaks_tests;

#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	#[test]
	fn human_bytes_and_short_shas_format_for_toasts() {
		assert_eq!(human_bytes(0), "0 B");
		assert_eq!(human_bytes(1023), "1023 B");
		assert_eq!(human_bytes(1536), "1.5 KB");
		assert_eq!(human_bytes(2250174), "2.1 MB");
		let shas: Vec<String> = ["a", "b", "c", "d", "e", "f"]
			.iter()
			.map(|c| c.repeat(40))
			.collect();
		assert_eq!(short_shas(&shas, 3), "aaaaaaaa, bbbbbbbb, cccccccc, +3");
		assert_eq!(
			short_shas(&shas, usize::MAX),
			"aaaaaaaa, bbbbbbbb, cccccccc, dddddddd, eeeeeeee, ffffffff"
		);
	}

	#[test]
	fn apply_status_of_an_old_peer_keeps_every_error_line() {
		let result = PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: 0,
				overwritten_count: 0,
				skipped_existing_count: 0,
				planned_skipped_count: 0,
				deleted_count: 0,
				errors: vec![
					"alpha broke".into(),
					"beta broke".into(),
					"gamma broke".into(),
				],
				failures: Vec::new(),
				..Default::default()
			},
			created_commits: Vec::new(),
		};
		let msg = result.status();
		for locale in [crate::i18n::Locale::ZhTw, crate::i18n::Locale::En] {
			let text = msg.render(locale);
			for line in ["alpha broke", "beta broke", "gamma broke"] {
				assert!(text.contains(line), "{line} missing: {text}");
			}
			assert!(!text.contains(": ;") && !text.contains(", ,"), "{text}");
			assert!(!text.ends_with("：") && !text.ends_with(": "), "{text}");
		}
	}

	#[test]
	fn apply_status_lists_each_failed_path_not_only_the_count() {
		use snip_core::reason::Reason;
		use snip_core::restore::RestoreFailure;
		let result = |failures: Vec<(&str, Reason)>| PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: 2,
				overwritten_count: 1,
				skipped_existing_count: 0,
				planned_skipped_count: 0,
				deleted_count: 0,
				errors: failures
					.iter()
					.map(|(p, _)| format!("{p}: english"))
					.collect(),
				failures: failures
					.into_iter()
					.map(|(path, r)| RestoreFailure {
						path: path.into(),
						reason: Some(r),
					})
					.collect(),
				..Default::default()
			},
			created_commits: Vec::new(),
		};
		let ok = result(Vec::new()).status();
		assert_eq!(ok.key, "status_paste_done");

		let partial = result(vec![
			(
				"a/x.txt",
				Reason::PermissionDenied {
					path: "a/x.txt".into(),
				},
			),
			(
				"b/y.txt",
				Reason::UnsafePath {
					path: "b/y.txt".into(),
				},
			),
		])
		.status();
		assert_eq!(partial.key, "status_paste_partial_more");
		let zh = partial.render(crate::i18n::Locale::ZhTw);
		let en = partial.render(crate::i18n::Locale::En);
		for text in [&zh, &en] {
			assert!(text.contains("a/x.txt"), "{text}");
			assert!(text.contains("b/y.txt"), "{text}");
			assert!(!text.contains("{}"), "{text}");
			assert!(!text.contains("english"), "{text}");
		}
		assert!(zh.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)));

		let one = result(vec![(
			"a/x.txt",
			Reason::UnsafePath {
				path: "a/x.txt".into(),
			},
		)])
		.status();
		assert_eq!(one.key, "status_paste_partial");
		assert!(one.render(crate::i18n::Locale::ZhTw).contains("a/x.txt"));
	}

	#[test]
	fn apply_status_keeps_path_and_cause_of_every_failure() {
		use snip_core::reason::Reason;
		use snip_core::restore::RestoreFailure;
		let failures = vec![
			RestoreFailure {
				path: "a.txt".into(),
				reason: Some(Reason::PermissionDenied {
					path: "a.txt".into(),
				}),
			},
			RestoreFailure {
				path: "b.txt".into(),
				reason: Some(Reason::Io {
					detail: snip_core::reason::Datum::for_test(
						"No space left on device",
					),
				}),
			},
			RestoreFailure {
				path: "c.txt".into(),
				reason: Some(Reason::UnsafePath {
					path: "c.txt".into(),
				}),
			},
		];
		let result = PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: 0,
				overwritten_count: 0,
				skipped_existing_count: 0,
				planned_skipped_count: 0,
				deleted_count: 0,
				errors: failures.iter().map(|f| f.path.clone()).collect(),
				failures: failures.clone(),
				..Default::default()
			},
			created_commits: Vec::new(),
		};
		let status = result.status();
		for loc in [crate::i18n::Locale::ZhTw, crate::i18n::Locale::En] {
			let text = status.render(loc);
			for f in &failures {
				let line = restore_failure_msg(f).render(loc);
				assert!(text.contains(&line), "{line} missing: {text}");
				assert!(text.contains(&f.path), "{text}");
			}
			assert!(text.contains("No space left on device"), "{text}");
			assert!(!text.contains("{}"), "{text}");
		}
		let zh = status.render(crate::i18n::Locale::ZhTw);
		let en = status.render(crate::i18n::Locale::En);
		assert_ne!(zh, en);
	}

	#[test]
	fn apply_status_of_thousands_of_failures_does_not_recurse() {
		use snip_core::reason::Reason;
		use snip_core::restore::RestoreFailure;
		const N: usize = 20_000;
		let failures: Vec<RestoreFailure> = (0..N)
			.map(|i| RestoreFailure {
				path: format!("f{i}.txt"),
				reason: Some(Reason::Io {
					detail: snip_core::reason::Datum::for_test(
						"No space left on device",
					),
				}),
			})
			.collect();
		let result = PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: 0,
				overwritten_count: 0,
				skipped_existing_count: 0,
				planned_skipped_count: 0,
				deleted_count: 0,
				errors: failures.iter().map(|f| f.path.clone()).collect(),
				failures,
				..Default::default()
			},
			created_commits: Vec::new(),
		};
		// Default test thread: any per-failure recursion overflows it.
		let status = result.status();
		let copy = status.clone();
		assert!(copy.heap_bytes() > 0);
		for loc in [crate::i18n::Locale::ZhTw, crate::i18n::Locale::En] {
			let text = status.render(loc);
			assert!(text.contains("f0.txt"), "{loc:?}");
			assert!(text.contains(&format!("f{}.txt", N - 1)), "{loc:?}");
		}
		drop(copy);
		drop(status);
	}

	#[test]
	fn a_missing_prefix_folder_is_named_not_the_destination() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("dest");
		std::fs::create_dir(&dest).unwrap();
		let bad = tmp.path().join("gone-prefix");
		{
			let route = PasteRoute {
				prefixes: vec![("src".into(), bad.clone())],
				..Default::default()
			};
			let err = PasteTarget::Local {
				dest: dest.clone(),
				bound: None,
			}
			.plan_files(
				"// clipcode-root: src\n// FILE: src/a.txt\nx\n",
				&route,
				&RunOptions::default(),
				None,
			)
			.expect_err("the prefix folder does not resolve");
			let msg = destination_msg(None, &dest, err);
			assert_eq!(msg.args[0], bad.display().to_string(), "{msg:?}");
			for loc in [crate::i18n::Locale::ZhTw, crate::i18n::Locale::En] {
				let text = msg.render(loc);
				assert!(text.contains(&bad.display().to_string()), "{text}");
				assert!(!text.contains(&dest.display().to_string()), "{text}");
			}
		}
	}

	/// The tree as text (a replay's too): `R label files`,
	/// `D name files d<depth>`, `F path d<depth>` and `C commit`, a `+` after
	/// a folded node.
	fn shape(plan: &PastePreviewPlan) -> Vec<String> {
		let nodes = if plan.whole_commit {
			plan.commit_rows()
		} else {
			plan.tree_rows()
		};
		nodes
			.iter()
			.map(|r| match r {
				PasteNode::Root {
					label,
					sum,
					collapsed,
					..
				} => format!(
					"R {label} {}{}",
					sum.files,
					if *collapsed { "+" } else { "" }
				),
				PasteNode::Dir {
					name,
					depth,
					sum,
					collapsed,
					..
				} => format!(
					"D {name} {} d{depth}{}",
					sum.files,
					if *collapsed { "+" } else { "" }
				),
				PasteNode::File(ix, depth) => {
					format!("F {} d{depth}", plan.items[*ix].path)
				}
				PasteNode::Commit(c) => format!("C {c}"),
			})
			.collect()
	}

	const SRC_TREE: &str = "// clipcode-root: src\n// FILE: src/app/a.ts\nA\n// FILE: src/app/b.ts\nB\n// FILE: src/lib/c.ts\nC\n";

	fn tree_plan(raw: &str) -> (tempfile::TempDir, PastePreviewPlan) {
		let dir = tempfile::tempdir().unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		(dir, plan)
	}

	/// The label of the (only) destination root a plan writes into.
	fn root_label(plan: &PastePreviewPlan) -> String {
		root_name(
			&plan
				.items
				.iter()
				.find(|i| !i.planned_skip())
				.unwrap()
				.dest_root,
		)
	}

	/// The folder `path` ("" is the root row) of the plan's first written root.
	fn key(plan: &PastePreviewPlan, path: &str) -> DirKey {
		DirKey {
			commit: None,
			root: plan
				.items
				.iter()
				.find(|i| !i.planned_skip())
				.unwrap()
				.dest_root
				.clone(),
			path: path.into(),
		}
	}

	fn ix_of(plan: &PastePreviewPlan, path: &str) -> usize {
		plan.items.iter().position(|i| i.path == path).unwrap()
	}

	fn sum_of(plan: &PastePreviewPlan, path: &str) -> DirSummary {
		plan.tree_rows()
			.into_iter()
			.find_map(|n| match n {
				PasteNode::Root { key, sum, .. }
				| PasteNode::Dir { key, sum, .. }
					if key.path == path =>
				{
					Some(sum)
				}
				_ => None,
			})
			.unwrap_or_else(|| panic!("no folder row {path:?}"))
	}

	/// Exported from the folder above the destination `sub`: the first path
	/// segment repeats the destination's name.
	const RESTORE_BASE_PAYLOAD: &str =
		"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n";

	fn item_paths(plan: &PastePreviewPlan) -> Vec<&str> {
		plan.items.iter().map(|i| i.path.as_str()).collect()
	}

	#[test]
	fn a_clipcode_root_paste_keeps_the_restore_base_suggestion() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("sub");
		fs::create_dir(&dest).unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			RESTORE_BASE_PAYLOAD,
			&dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert_eq!(
			plan.restore_hint().map(|s| &s.base),
			Some(&RestoreBase::Strip {
				segment: "sub".into()
			})
		);
		assert!(!plan.adjust_paths);
		assert_eq!(item_paths(&plan), ["sub/a.ts"]);
	}

	/// A retarget starts the tree and the restore-base hint over: the shell
	/// shown while the new folder is planned keeps no fold, folder cursor,
	/// hint or [Adjust paths] of the old one, and the new plan is not
	/// adjusted unasked (#238 over #239 / #240).
	#[test]
	fn retarget_starts_the_tree_and_the_hint_over() {
		let tmp = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(tmp.path()).unwrap();
		let dest = root.join("sub");
		let other = root.join("other");
		fs::create_dir(&dest).unwrap();
		fs::create_dir(&other).unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n// FILE: sub/d/b.ts\ny\n",
			&dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		plan.set_adjust_paths(true).unwrap();
		assert!(plan.restore_hint().is_some());
		let (root_key, path, _) = plan
			.all_dirs()
			.into_iter()
			.find(|(_, path, _)| !path.is_empty())
			.expect("a folder row");
		let key = DirKey {
			commit: None,
			root: root_key,
			path,
		};
		plan.collapsed_dirs.insert(key.clone());
		plan.cursor_dir = Some(key);

		plan.disarm_for(other.clone());
		assert!(plan.restore_hint().is_none(), "the old folder's hint");
		assert!(!plan.adjust_paths);
		assert!(plan.collapsed_dirs.is_empty());
		assert!(plan.cursor_dir.is_none());
		assert_eq!(plan.cursor(), PasteCursor::Item(0));

		let plan = plan
			.retarget_with(
				&other,
				&[],
				crate::gen::Stamp::for_tests(2),
				&RunOptions::default(),
				MAX_RETAINED_PREVIEW_BYTES,
			)
			.unwrap();
		assert_eq!(plan.destination, other);
		assert!(!plan.adjust_paths, "nothing adjusted unasked");
		assert_eq!(item_paths(&plan), ["sub/a.ts", "sub/d/b.ts"]);
		assert!(plan.cursor_dir.is_none());
	}

	#[test]
	fn adjusting_paths_replans_with_the_suggestion_and_as_is_undoes_it() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = dunce::canonicalize(tmp.path()).unwrap().join("sub");
		fs::create_dir(&dest).unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			RESTORE_BASE_PAYLOAD,
			&dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		plan.set_adjust_paths(true).unwrap();
		assert_eq!(item_paths(&plan), ["a.ts"]);
		assert_eq!(plan.items[0].dest_path, dest.join("a.ts"));
		// Apply plans from the same route as the preview did.
		assert!(plan.target_route().unwrap().1.adjust_paths);
		// The hint stays so the choice can be undone.
		assert!(plan.restore_hint().is_some());
		plan.set_adjust_paths(false).unwrap();
		assert_eq!(item_paths(&plan), ["sub/a.ts"]);
		assert!(!plan.adjust_paths);
		assert!(!plan.target_route().unwrap().1.adjust_paths);
	}

	#[test]
	fn an_add_suggestion_nests_everything_under_the_existing_folder() {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		fs::create_dir_all(ws.join("app")).unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// clipcode-root: app\n// FILE: src/a.ts\nx\n",
			&ws,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert_eq!(
			plan.restore_hint().map(|s| &s.base),
			Some(&RestoreBase::Add {
				prefix: "app".into()
			})
		);
		plan.set_adjust_paths(true).unwrap();
		assert_eq!(item_paths(&plan), ["app/src/a.ts"]);
	}

	#[test]
	fn prefix_rows_hide_the_heuristic_suggestion() {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		fs::create_dir_all(ws.join("src/a")).unwrap();
		fs::create_dir_all(ws.join("src/b")).unwrap();
		let text = "// FILE: a/x.ts\nx\n// FILE: b/y.ts\ny\n";
		// The fixture is one the core heuristic does answer.
		let paths = ["a/x.ts".to_string(), "b/y.ts".to_string()];
		assert!(matches!(
			snip_core::restore::suggest_restore_base(
				&ws.to_string_lossy(),
				&paths,
				&snip_core::restore::FsProbe,
				None,
			),
			Some(RestoreBaseSuggestion {
				base: RestoreBase::Add { .. },
				..
			})
		));
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			text,
			&ws,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		plan.set_keep_relative("a").unwrap();
		plan.set_keep_relative("b").unwrap();
		assert!(plan.restore_hint().is_none());
		let err = plan.choose_adjust_paths(true).unwrap_err();
		assert_eq!(err.key, "paste_adjust_unavailable");
	}

	#[test]
	fn a_commit_payload_has_no_restore_hint() {
		let tmp = tempfile::tempdir().unwrap();
		let path = tmp.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_modifying_a(),
			&path,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert!(plan.whole_commit);
		assert!(plan.restore_hint().is_none());
		assert!(plan.choose_adjust_paths(true).is_err());
	}

	#[test]
	fn change_tree_groups_by_root_and_directory_in_display_order() {
		let dir = tempfile::tempdir().unwrap();
		let raw = "// clipcode-root: r\n// FILE: r/b/z.txt\n1\n// FILE: r/a.txt\n2\n// FILE: r/b/y.txt\n3\n// FILE: r/c/d/e.txt\n4\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let root = root_label(&plan);
		assert_eq!(
			shape(&plan),
			[
				format!("R {root} 4"),
				"D r 4 d1".into(),
				"D b 2 d2".into(),
				"F r/b/y.txt d3".into(),
				"F r/b/z.txt d3".into(),
				"D c/d 1 d2".into(),
				"F r/c/d/e.txt d3".into(),
				"F r/a.txt d2".into(),
			]
		);
		// Up / Down walk the rows on screen, folders included.
		plan.selected_item_idx = ix_of(&plan, "r/b/y.txt");
		plan.select_next();
		assert_eq!(plan.cursor(), PasteCursor::Item(ix_of(&plan, "r/b/z.txt")));
		plan.select_next();
		assert_eq!(plan.cursor(), PasteCursor::Dir(key(&plan, "r/c/d")));
		plan.select_next();
		assert_eq!(
			plan.cursor(),
			PasteCursor::Item(ix_of(&plan, "r/c/d/e.txt"))
		);
		plan.select_next();
		assert_eq!(plan.cursor(), PasteCursor::Item(ix_of(&plan, "r/a.txt")));
		plan.select_next();
		assert_eq!(
			plan.cursor(),
			PasteCursor::Item(ix_of(&plan, "r/a.txt")),
			"the last row stays"
		);
		plan.select_prev();
		assert_eq!(
			plan.cursor(),
			PasteCursor::Item(ix_of(&plan, "r/c/d/e.txt"))
		);
	}

	/// #239: a path with several folders is a tree, not one flat folder row.
	#[test]
	fn paste_tree_nests_directories() {
		let (_dir, plan) = tree_plan(SRC_TREE);
		let root = root_label(&plan);
		assert_eq!(
			shape(&plan),
			[
				format!("R {root} 3"),
				"D src 3 d1".into(),
				"D app 2 d2".into(),
				"F src/app/a.ts d3".into(),
				"F src/app/b.ts d3".into(),
				"D lib 1 d2".into(),
				"F src/lib/c.ts d3".into(),
			]
		);
		let order: Vec<&str> = plan
			.display_order()
			.into_iter()
			.map(|ix| plan.items[ix].path.as_str())
			.collect();
		assert_eq!(order, ["src/app/a.ts", "src/app/b.ts", "src/lib/c.ts"]);
	}

	#[test]
	fn paste_tree_compresses_single_child_chains_and_counts_actions() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
		fs::write(dir.path().join("a/b/c/x.ts"), "old\n").unwrap();
		let raw = "// clipcode-root: a\n// FILE: a/b/c/x.ts\nX\n// FILE: a/b/c/y.ts\nY\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let root = root_label(&plan);
		assert_eq!(
			shape(&plan),
			[
				format!("R {root} 2"),
				"D a/b/c 2 d1".into(),
				"F a/b/c/x.ts d2".into(),
				"F a/b/c/y.ts d2".into(),
			]
		);
		let folder = key(&plan, "a/b/c");
		assert_eq!(
			plan.counts_under(&folder),
			PasteCounts {
				creates: 1,
				overwrites: 0,
				existing: 1,
				deletes: 0,
				skips: 1,
			}
		);
		let x = ix_of(&plan, "a/b/c/x.ts");
		plan.toggle_overwrite(x);
		assert_eq!(
			plan.counts_under(&folder),
			PasteCounts {
				creates: 1,
				overwrites: 1,
				existing: 1,
				deletes: 0,
				skips: 0,
			}
		);
		assert_eq!(sum_of(&plan, "a/b/c").tick(), Tick::All);
	}

	#[test]
	fn skipped_rows_stay_at_the_root_level_with_their_raw_path() {
		let (_dir, plan) = tree_plan(
			"// clipcode-root: p\n// FILE: p/a.txt\n1\n// FILE: .git/config\n2\n// FILE: p/x/.git/hooks/h\n3\n",
		);
		assert!(
			plan.items.iter().any(|i| i.planned_skip()),
			"the protected paths are skipped rows"
		);
		let root = root_label(&plan);
		assert_eq!(
			shape(&plan),
			[
				format!("R {root} 3"),
				"D p 1 d1".into(),
				"F p/a.txt d2".into(),
				"F .git/config d1".into(),
				"F p/x/.git/hooks/h d1".into(),
			]
		);
	}

	#[test]
	fn same_named_roots_stay_apart() {
		let (_dir, mut plan) = tree_plan(SRC_TREE);
		let other = tempfile::tempdir().unwrap();
		let c = ix_of(&plan, "src/lib/c.ts");
		let name = plan.items[c].dest_root_name.clone();
		plan.items[c].dest_root = other.path().join(&name);
		let mut roots: Vec<usize> = shape(&plan)
			.iter()
			.filter_map(|l| l.strip_prefix("R "))
			.map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
			.collect();
		roots.sort_unstable();
		assert_eq!(roots, [1, 2], "{:?}", shape(&plan));
	}

	#[cfg(unix)]
	#[test]
	fn a_symlinked_destination_is_one_root() {
		let tmp = tempfile::tempdir().unwrap();
		let real = tmp.path().join("real").join("web");
		fs::create_dir_all(&real).unwrap();
		fs::create_dir(tmp.path().join("links")).unwrap();
		let link = tmp.path().join("links").join("web");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			"// clipcode-root: p\n// FILE: p/a.txt\n1\n// FILE: .git/config\n2\n",
			&link,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let roots = shape(&plan).iter().filter(|l| l.starts_with("R ")).count();
		assert_eq!(roots, 1, "{:?}", shape(&plan));
	}

	/// A delete's path is resolved by the engine too: through the symlinked
	/// destination it must still land under the one root, beside the writes.
	#[cfg(unix)]
	#[test]
	fn a_delete_under_a_symlinked_destination_stays_in_its_root() {
		let tmp = tempfile::tempdir().unwrap();
		let real = tmp.path().join("real").join("web");
		fs::create_dir_all(real.join("p")).unwrap();
		fs::write(real.join("p").join("gone.txt"), "old\n").unwrap();
		fs::create_dir(tmp.path().join("links")).unwrap();
		let link = tmp.path().join("links").join("web");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			"// clipcode-root: p\n// FILE: p/a.txt\n1\n// FILE: [DELETED] p/gone.txt\n// This file has been deleted in this change\n",
			&link,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let gone = ix_of(&plan, "p/gone.txt");
		assert!(plan.items[gone].is_delete(), "{:?}", plan.items[gone].op);
		assert_eq!(
			shape(&plan),
			[
				format!("R {} 2", root_label(&plan)),
				"D p 2 d1".to_string(),
				"F p/a.txt d2".to_string(),
				"F p/gone.txt d2".to_string(),
			]
		);
		assert_eq!(plan.counts().deletes, 1);
	}

	#[test]
	fn a_rows_root_is_the_one_its_relative_path_is_relative_to() {
		let roots: Vec<PathBuf> =
			["/w/dest", "/w/dest/sub"].map(Into::into).into();
		let of = |abs: &str, rel: &str| {
			plan_root_of(&roots, Path::new(abs), rel).cloned()
		};
		assert_eq!(
			of("/w/dest/sub/g.txt", "sub/g.txt"),
			Some(roots[0].clone())
		);
		assert_eq!(of("/w/dest/sub/g.txt", "g.txt"), Some(roots[1].clone()));
		assert_eq!(of("/w/dest/g.txt", "g.txt"), Some(roots[0].clone()));
		// No root spelled that way: the caller falls back.
		assert_eq!(of("/w/other/g.txt", "g.txt"), None);
		assert_eq!(of("/w/dest/subg.txt", "g.txt"), None);
		// A worker's Windows spelling.
		let win: Vec<PathBuf> =
			[r"C:\w\dest", r"C:\w\dest\sub"].map(Into::into).into();
		assert_eq!(
			plan_root_of(&win, Path::new(r"C:\w\dest\sub\g.txt"), "g.txt"),
			Some(&win[1])
		);
	}

	#[test]
	fn a_delete_belongs_to_the_innermost_root() {
		let roots: Vec<PathBuf> = ["/w/dest", "/w/dest/sub", "/w/other"]
			.map(Into::into)
			.into();
		let of = |p: &str| innermost_root(&roots, Path::new(p));
		assert_eq!(of("/w/dest/a.txt"), Some(&roots[0]));
		assert_eq!(of("/w/dest/sub/a.txt"), Some(&roots[1]));
		assert_eq!(of("/w/dest/subway/a.txt"), Some(&roots[0]));
		assert_eq!(of("/w/other/x"), Some(&roots[2]));
		assert_eq!(of("/elsewhere/x"), None);
	}

	/// A Windows worker's roots, on a master of any OS: a delete's root and
	/// a root's label are read from the worker's spelling, not taken apart
	/// by this machine's path rules (on Unix `\` is part of a name).
	#[test]
	fn a_windows_workers_roots_are_named_and_matched_on_any_master() {
		let roots: Vec<PathBuf> =
			[r"C:\w\dest", r"C:\w\dest\sub", r"C:\w\other"]
				.map(Into::into)
				.into();
		let of = |p: &str| innermost_root(&roots, Path::new(p));
		assert_eq!(of(r"C:\w\dest\a.txt"), Some(&roots[0]));
		assert_eq!(of(r"C:\w\dest\sub\a.txt"), Some(&roots[1]));
		assert_eq!(of(r"C:\w\dest\subway\a.txt"), Some(&roots[0]));
		assert_eq!(of(r"C:\w\other\x"), Some(&roots[2]));
		assert_eq!(of(r"C:\elsewhere\x"), None);
		assert_eq!(root_name(Path::new(r"C:\w\dest")), "dest");
		assert_eq!(root_name(Path::new(r"C:\w\dest\")), "dest");
		assert_eq!(root_name(Path::new("/w/dest")), "dest");
	}

	/// A row's place in the tree is the place Apply acts on: its root joined
	/// with its path is its destination file, for a create, an overwrite and a
	/// delete alike, even where one destination root lies inside another.
	/// The folder row that holds a row's path then holds the row, so
	/// unchecking it unchecks the create and the delete beside each other.
	#[test]
	fn a_row_in_a_nested_root_sits_where_apply_acts_and_follows_its_folder() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = dunce::canonicalize(tmp.path()).unwrap().join("dest");
		let sub = dest.join("sub");
		fs::create_dir_all(&sub).unwrap();
		fs::write(sub.join("gone.txt"), "old\n").unwrap();
		fs::write(sub.join("over.txt"), "old\n").unwrap();
		// `p/` is routed into dest/sub; `sub/` keeps its relative path
		// under dest, so those rows are written into the same folder from
		// the outer root.
		let raw = "// FILE: p/a.txt\nA\n\
			// FILE: sub/new.txt\nN\n\
			// FILE: sub/over.txt\nO\n\
			// FILE: [DELETED] sub/gone.txt\n// This file has been deleted in this change\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			&dest,
			&[dest.clone(), sub.clone()],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		plan.choose_prefix_destination("p", &sub).unwrap();
		plan.choose_keep_relative("sub").unwrap();
		plan.rebuild_file_plan_with(&RunOptions::default()).unwrap();

		let written: Vec<&PasteItem> =
			plan.items.iter().filter(|i| !i.planned_skip()).collect();
		assert_eq!(written.len(), 4, "{:?}", plan.items);
		for item in &written {
			assert_eq!(
				item.dest_root.join(&item.path),
				item.dest_path,
				"{:?} {:?} is shown where Apply does not act",
				item.op,
				item.path
			);
		}
		let gone = &plan.items[ix_of(&plan, "sub/gone.txt")];
		assert!(gone.is_delete());
		assert_eq!(gone.dest_root, dest);

		let folder = DirKey {
			commit: None,
			root: dest.clone(),
			path: "sub".into(),
		};
		assert_eq!(plan.set_dir_selected(&folder), Some(false));
		let selected = |path: &str| plan.items[ix_of(&plan, path)].selected;
		assert!(!selected("sub/new.txt"));
		assert!(!selected("sub/over.txt"));
		assert!(
			!selected("sub/gone.txt"),
			"the folder's own delete stays checked beside its creates"
		);
		assert!(
			selected("a.txt"),
			"a row of the inner root is not in dest/sub"
		);
		assert_eq!(plan.counts().deletes, 0);
	}

	#[test]
	fn folder_include_is_tri_state_and_ignores_sibling_prefixes() {
		let (_dir, mut plan) = tree_plan(
			"// clipcode-root: src\n// FILE: src/a/y.ts\nY\n// FILE: src/ab/x.ts\nX\n// FILE: src/a/z.ts\nZ\n// FILE: .git/config\nG\n",
		);
		let sel = |plan: &PastePreviewPlan, path: &str| {
			plan.items[ix_of(plan, path)].selected
		};
		assert_eq!(plan.set_dir_selected(&key(&plan, "src/a")), Some(false));
		assert!(!sel(&plan, "src/a/y.ts") && !sel(&plan, "src/a/z.ts"));
		assert!(sel(&plan, "src/ab/x.ts"), "src/ab is not under src/a");
		assert_eq!(sum_of(&plan, "src").tick(), Tick::Partial);
		assert_eq!(sum_of(&plan, "src/a").tick(), Tick::Empty);
		assert_eq!(plan.set_dir_selected(&key(&plan, "src")), Some(true));
		assert!(["src/a/y.ts", "src/a/z.ts", "src/ab/x.ts"]
			.iter()
			.all(|p| sel(&plan, p)));
		// The root row never reaches a row that was skipped when planned.
		let root = key(&plan, "");
		assert_eq!(plan.set_dir_selected(&root), Some(false));
		assert_eq!(plan.set_dir_selected(&root), Some(true));
		assert!(!sel(&plan, ".git/config"));
		assert_eq!(plan.counts().creates, 3);
	}

	#[test]
	fn folding_a_folder_moves_the_cursor_onto_it_and_steps_skip_hidden_rows() {
		let (_dir, mut plan) = tree_plan(SRC_TREE);
		let app = key(&plan, "src/app");
		let b = ix_of(&plan, "src/app/b.ts");
		plan.selected_item_idx = b;
		assert_eq!(
			plan.toggle_dir_collapsed(&app),
			Some(PasteCursor::Dir(app.clone()))
		);
		assert_eq!(
			plan.cursor(),
			PasteCursor::Item(b),
			"folding does not move the cursor itself"
		);
		// The session applies the move.
		plan.cursor_dir = Some(app.clone());
		let order: Vec<&str> = plan
			.display_order()
			.into_iter()
			.map(|ix| plan.items[ix].path.as_str())
			.collect();
		assert_eq!(order, ["src/lib/c.ts"]);
		assert_eq!(
			plan.step_target(true),
			PasteCursor::Dir(key(&plan, "src/lib"))
		);
		assert_eq!(
			plan.step_target(false),
			PasteCursor::Dir(key(&plan, "src"))
		);
		// A cursor that stays visible is left alone.
		let (_dir, mut plan) = tree_plan(SRC_TREE);
		plan.selected_item_idx = ix_of(&plan, "src/lib/c.ts");
		assert_eq!(plan.toggle_dir_collapsed(&key(&plan, "src/app")), None);
	}

	#[test]
	fn left_and_right_walk_the_tree() {
		let (_dir, mut plan) = tree_plan(SRC_TREE);
		let (lib, root) = (key(&plan, "src/lib"), key(&plan, ""));
		let c = ix_of(&plan, "src/lib/c.ts");
		plan.selected_item_idx = c;
		assert_eq!(
			plan.nav_left(),
			TreeStep::Move(PasteCursor::Dir(lib.clone()))
		);
		assert_eq!(
			plan.nav_right(),
			TreeStep::Nothing,
			"a file has no children"
		);
		plan.cursor_dir = Some(lib.clone());
		assert_eq!(plan.nav_left(), TreeStep::Fold(lib.clone()));
		assert_eq!(plan.toggle_dir_collapsed(&lib), None);
		assert_eq!(plan.nav_right(), TreeStep::Unfold(lib.clone()));
		assert_eq!(
			plan.nav_left(),
			TreeStep::Move(PasteCursor::Dir(key(&plan, "src"))),
			"a folded folder goes up to its parent"
		);
		assert_eq!(plan.toggle_dir_collapsed(&lib), None);
		assert_eq!(plan.nav_right(), TreeStep::Move(PasteCursor::Item(c)));
		plan.cursor_dir = Some(root.clone());
		assert_eq!(plan.nav_left(), TreeStep::Fold(root.clone()));
		assert_eq!(plan.toggle_dir_collapsed(&root), None);
		assert_eq!(
			plan.nav_left(),
			TreeStep::Nothing,
			"nothing above the root"
		);
		assert_eq!(plan.nav_right(), TreeStep::Unfold(root));

		// A commit replay is a flat list under its headers.
		let (_dir, plan) = mixed_plan();
		assert_eq!(plan.nav_left(), TreeStep::Nothing);
		assert_eq!(plan.nav_right(), TreeStep::Nothing);
	}

	#[test]
	fn big_previews_open_with_the_first_level_folded() {
		let raw = |n: usize| {
			let mut raw = String::from("// clipcode-root: p\n");
			for i in 0..n {
				raw.push_str(&format!("// FILE: p/d{i}/f.txt\nx\n"));
			}
			raw
		};
		let (_dir, plan) = tree_plan(&raw(201));
		assert_eq!(
			shape(&plan),
			[format!("R {} 201", root_label(&plan)), "D p 201 d1+".into()]
		);
		assert_eq!(plan.cursor(), PasteCursor::Dir(key(&plan, "p")));
		assert!(plan.detail_preview().is_none());

		let (_dir, plan) = tree_plan(&raw(200));
		assert!(plan.collapsed_dirs.is_empty());
		assert!(!shape(&plan).iter().any(|l| l.ends_with('+')));
	}

	#[test]
	fn a_remap_keeps_folds_of_folders_that_still_exist() {
		let dir = tempfile::tempdir().unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: a/x/1.txt\n1\n// FILE: b/y/2.txt\n2\n",
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		plan.choose_keep_relative("a").unwrap();
		plan.choose_keep_relative("b").unwrap();
		plan.rebuild_file_plan_with(&RunOptions::default()).unwrap();
		let (ax, by) = (key(&plan, "a/x"), key(&plan, "b/y"));
		assert_eq!(
			plan.toggle_dir_collapsed(&ax),
			Some(PasteCursor::Dir(ax.clone()))
		);
		plan.cursor_dir = Some(ax.clone());
		assert_eq!(plan.toggle_dir_collapsed(&by), None);
		plan.cursor_dir = Some(by.clone());

		plan.choose_keep_relative("a").unwrap();
		plan.rebuild_file_plan_with(&RunOptions::default()).unwrap();
		plan.normalize_cursor();
		assert!(plan.collapsed_dirs.contains(&ax));
		assert!(plan.collapsed_dirs.contains(&by));
		assert_eq!(plan.cursor_dir, Some(by));

		let gone = key(&plan, "gone/dir");
		plan.collapsed_dirs.insert(gone.clone());
		plan.normalize_cursor();
		assert!(!plan.collapsed_dirs.contains(&gone));
		assert!(plan.collapsed_dirs.contains(&ax));
	}

	#[test]
	fn dir_control_ids_keep_the_driver_shape() {
		assert_eq!(
			dir_control_id(DirControl::Row, 0, "長路徑/a b"),
			"paste-dir:0:長路徑/a b"
		);
		assert_eq!(
			dir_control_id(DirControl::Toggle, 1, ""),
			"paste-dir-toggle:1:"
		);
		assert_eq!(
			dir_control_id(DirControl::Include, 0, "src"),
			"paste-dir-include:0:src"
		);
	}

	use std::fs;

	/// A remote Apply whose answer never came may have written: it is
	/// reported as unknown, never as a refused destination.
	#[test]
	fn a_lost_answer_after_a_remote_apply_is_unknown_not_refused() {
		use snip_remote::{ErrorCode, RemoteError};
		let worker = Arc::new(snip_remote::Worker::new(Default::default()));
		let remote = RemotePaste {
			session: crate::remote::RemoteSession::new(
				snip_remote::RemoteHost::in_process(worker),
				snip_remote::RemoteWorkspace {
					id: "/w".into(),
					name: "w".into(),
					path: "/w".into(),
				},
			),
		};
		let dest = remote.session.root.clone();
		let lost = || {
			RemoteError::Io(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"the worker closed the connection",
			))
		};
		assert_eq!(
			file_apply_msg(
				Some(&remote),
				&dest,
				PasteError::OutcomeUnknown(lost())
			)
			.key,
			"paste_outcome_unknown"
		);
		assert_eq!(
			file_apply_msg(
				Some(&remote),
				&dest,
				PasteError::OutcomeUnknown(RemoteError::TimedOut)
			)
			.key,
			"paste_outcome_unknown"
		);
		let stale = RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "stale destination in '/w': target file 'a' was created \
			          externally after preview"
				.into(),
			reason: Some(Box::new(snip_remote::Reason::DestinationChanged {
				change: snip_remote::StaleChange::Created { rel: "a".into() },
			})),
			suggestion: None,
		};
		assert_eq!(
			file_apply_msg(Some(&remote), &dest, PasteError::Remote(stale)).key,
			"stale_created"
		);
		// A worker that sends no reason (an old one) still refuses as stale,
		// worded by its code; the kind of change is not guessed from its
		// sentence.
		let old = RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "stale destination in '/w': target file 'a' was created \
			          externally after preview"
				.into(),
			reason: None,
			suggestion: None,
		};
		assert_eq!(
			file_apply_msg(Some(&remote), &dest, PasteError::Remote(old)).key,
			"remote_refused_stale"
		);
		// Never reached the worker: nothing can have been written.
		assert_eq!(
			file_apply_msg(
				Some(&remote),
				&dest,
				PasteError::Remote(RemoteError::Start {
					kind: snip_remote::ConnectFailureKind::Other,
					reason: None,
					detail: snip_core::reason::Datum::for_test("no route"),
					message: "no route".into(),
				})
			)
			.key,
			"paste_err_destination"
		);
	}

	/// A known start failure words itself when drawn, in either language,
	/// with a key login's transport detail kept.
	#[test]
	fn a_start_failure_in_a_destination_message_follows_the_drawing_language() {
		use snip_core::gitsrc::StartReason;
		use snip_remote::{ConnectFailureKind, RemoteError};
		let start = |reason, detail: &str| {
			destination_msg(
				None,
				Path::new("/w"),
				PasteError::Remote(RemoteError::Start {
					kind: ConnectFailureKind::Other,
					reason: Some(reason),
					detail: snip_core::reason::Datum::for_test(detail),
					message: "english".into(),
				}),
			)
		};
		let no_worker = start(StartReason::NoWorker, "");
		assert!(no_worker.render(Locale::ZhTw).contains("沒有安裝 snip"));
		assert!(no_worker.render(Locale::En).contains("is not installed"));
		let login = start(StartReason::KeyLogin, "Permission denied");
		for loc in [Locale::ZhTw, Locale::En] {
			let text = login.render(loc);
			assert!(text.contains("Permission denied"), "{text}");
			assert!(text.contains("/w"), "{text}");
			assert_ne!(
				text,
				login.render(if loc == Locale::En {
					Locale::ZhTw
				} else {
					Locale::En
				})
			);
		}
	}

	#[test]
	fn file_apply_failures_keep_their_messages() {
		use snip_remote::{ErrorCode, RemoteError};
		let worker = Arc::new(snip_remote::Worker::new(Default::default()));
		let remote = RemotePaste {
			session: crate::remote::RemoteSession::new(
				snip_remote::RemoteHost::in_process(worker),
				snip_remote::RemoteWorkspace {
					id: "/w".into(),
					name: "w".into(),
					path: "/w".into(),
				},
			),
		};
		let dest = remote.session.root.clone();
		let busy_git =
			|| TransferError::Git(GitError::WorktreeBusy { args: "x".into() });
		let key = |r: Option<&RemotePaste>, e| file_apply_msg(r, &dest, e).key;
		assert_eq!(key(None, PasteError::Local(busy_git())), "paste_repo_busy");
		assert_eq!(
			key(
				Some(&remote),
				PasteError::Remote(RemoteError::Refused {
					code: ErrorCode::Busy,
					message: busy_git().paste_message(),
					reason: Some(Box::new(snip_remote::Reason::RepoBusy)),
					suggestion: None,
				})
			),
			"paste_repo_busy"
		);
		// The same words from a worker that sends no reason are a plain
		// busy refusal: nothing reads the sentence.
		assert_eq!(
			key(
				Some(&remote),
				PasteError::Remote(RemoteError::Refused {
					code: ErrorCode::Busy,
					message: busy_git().paste_message(),
					reason: None,
					suggestion: None,
				})
			),
			"paste_err_destination"
		);
		assert_eq!(
			key(
				Some(&remote),
				PasteError::Remote(RemoteError::Refused {
					code: ErrorCode::Busy,
					message: "too many callers".into(),
					reason: None,
					suggestion: None,
				})
			),
			"paste_err_destination"
		);
		assert_eq!(
			key(
				None,
				PasteError::Local(TransferError::StaleDestination {
					root: "/r".into(),
					reason:
						"target file 'a' was deleted externally after preview"
							.into(),
					change: snip_core::reason::StaleChange::Deleted {
						rel: "a".into()
					}
				})
			),
			"stale_deleted"
		);
		// A local error that is not a stale one is worded by its reason, not
		// as an external modification and not by its Display.
		let special = file_apply_msg(
			None,
			&dest,
			PasteError::Local(TransferError::SpecialFile("p".into())),
		);
		assert_eq!(
			special,
			reason_msg(&snip_core::reason::Reason::NotRegularFile {
				path: "p".into()
			})
		);
	}

	#[test]
	fn commit_failures_keep_their_messages() {
		use snip_remote::{ErrorCode, RemoteError};
		let worker = Arc::new(snip_remote::Worker::new(Default::default()));
		let remote = RemotePaste {
			session: crate::remote::RemoteSession::new(
				snip_remote::RemoteHost::in_process(worker),
				snip_remote::RemoteWorkspace {
					id: "/w".into(),
					name: "w".into(),
					path: "/w".into(),
				},
			),
		};
		let dest = remote.session.root.clone();
		let busy_git =
			|| TransferError::Git(GitError::WorktreeBusy { args: "x".into() });
		let key = |r: Option<&RemotePaste>, e, applying| {
			commit_msg(r, &dest, e, applying).key
		};
		let refused = |code, message: String| {
			PasteError::Remote(RemoteError::Refused {
				code,
				message,
				reason: None,
				suggestion: None,
			})
		};
		// Kept on purpose (design Risk 1): a worker's Busy refusal of a replay
		// reads as a repository error, not as paste_repo_busy. Changing this
		// is a decision, not a fix.
		assert_eq!(
			key(
				Some(&remote),
				refused(ErrorCode::Busy, busy_git().paste_message()),
				true
			),
			"error_open_repo"
		);
		assert_eq!(
			key(
				Some(&remote),
				PasteError::Remote(RemoteError::Refused {
					code: ErrorCode::Stale,
					message: "stale destination in '/w': target file 'a' was created externally after preview"
						.into(),
					reason: Some(Box::new(
						snip_remote::Reason::DestinationChanged {
							change: snip_remote::StaleChange::Created {
								rel: "a".into()
							}
						}
					)),
					suggestion: None,
				}),
				false
			),
			"stale_created"
		);
		assert_eq!(
			key(
				Some(&remote),
				PasteError::OutcomeUnknown(RemoteError::TimedOut),
				true
			),
			"paste_outcome_unknown"
		);
		assert_eq!(
			key(
				Some(&remote),
				PasteError::Remote(RemoteError::Start {
					kind: snip_remote::ConnectFailureKind::Other,
					reason: None,
					detail: snip_core::reason::Datum::for_test("no route"),
					message: "no route".into(),
				}),
				false
			),
			"paste_err_destination"
		);
		assert_eq!(
			key(Some(&remote), PasteError::NoReplayResult, true),
			"paste_err_destination"
		);
		for applying in [false, true] {
			assert_eq!(
				key(None, PasteError::Local(busy_git()), applying),
				"paste_repo_busy"
			);
		}
		let cancelled = || {
			PasteError::Local(TransferError::Git(GitError::Cancelled {
				args: "x".into(),
			}))
		};
		assert_eq!(key(None, cancelled(), true), "error_open_repo");
		assert_eq!(key(None, cancelled(), false), "err_cancelled");
	}

	/// Every user-facing surface that shows a remote refusal words it by
	/// its code; the worker's OS or protocol English never reaches the
	/// screen in any language (#213). The match is exhaustive, so a new
	/// `ErrorCode` must be decided here.
	#[test]
	fn remote_refusals_are_worded_on_every_paste_surface() {
		use snip_remote::{ErrorCode as C, RemoteError};
		fn worded(code: C) -> bool {
			match code {
				C::Forbidden
				| C::NotFound
				| C::Unsupported
				| C::BadRequest
				| C::TooLarge
				| C::Io
				| C::VersionMismatch
				| C::NotARepository
				| C::InvalidRevision
				| C::Timeout
				| C::Busy
				| C::Cancelled
				| C::OutsideShare => true,
				// The local paste's own wording stays as sent.
				C::Stale | C::Collision => false,
			}
		}
		let all = [
			C::Forbidden,
			C::NotFound,
			C::Unsupported,
			C::BadRequest,
			C::TooLarge,
			C::Io,
			C::VersionMismatch,
			C::NotARepository,
			C::InvalidRevision,
			C::Timeout,
			C::Busy,
			C::Cancelled,
			C::OutsideShare,
			C::Stale,
			C::Collision,
		];
		let worker = Arc::new(snip_remote::Worker::new(Default::default()));
		let remote = RemotePaste {
			session: crate::remote::RemoteSession::new(
				snip_remote::RemoteHost::in_process(worker),
				snip_remote::RemoteWorkspace {
					id: "/w".into(),
					name: "w".into(),
					path: "/w".into(),
				},
			),
		};
		let dest = remote.session.root.join("sub");
		let raw = "No such file or directory (os error 2)";
		let refused = |code| RemoteError::Refused {
			code,
			message: raw.into(),
			reason: None,
			suggestion: None,
		};
		for code in all {
			if !worded(code) {
				continue;
			}
			let surfaces: Vec<(&str, Msg)> = vec![
				("describe_msg", crate::remote::describe_msg(refused(code))),
				(
					"describe_msg_at",
					crate::remote::describe_msg_at(refused(code), "/srv/x"),
				),
				(
					"destination_msg",
					destination_msg(
						Some(&remote),
						&dest,
						PasteError::Remote(refused(code)),
					),
				),
				(
					"file_apply_msg",
					file_apply_msg(
						Some(&remote),
						&dest,
						PasteError::Remote(refused(code)),
					),
				),
				(
					"commit_msg check",
					commit_msg(
						Some(&remote),
						&dest,
						PasteError::Remote(refused(code)),
						false,
					),
				),
				(
					"commit_msg replay",
					commit_msg(
						Some(&remote),
						&dest,
						PasteError::Remote(refused(code)),
						true,
					),
				),
			];
			for (name, msg) in surfaces {
				for loc in [Locale::ZhTw, Locale::En] {
					let text = msg.render(loc);
					assert!(
						!text.contains("os error")
							&& !text.contains("No such file"),
						"{name} {code:?} {loc:?} shows worker text: {text}"
					);
				}
			}
		}
	}

	fn captured(text: String, dest: &Path) -> PasteRequest {
		PasteRequest::Clipboard {
			text,
			dest: dest.into(),
			roots: Vec::new(),
			generation: crate::gen::Stamp::for_tests(1),
			remote: None,
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
		let mut life =
			crate::lifecycle::Lifecycle::new(crate::gen::Gen::starting_at(1));
		let (id, held) = life.register_read(Some(token.clone()));
		let input_bytes = lock_pending(&pool).input.as_ref().unwrap().1;
		token.cancel();
		for n in 1..=100 {
			let mut pending = lock_pending(&pool);
			pending.invalidate(crate::gen::Stamp::for_tests(n));
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
					pending.ui_raw.as_ref(),
					pending.ui_commit.as_ref()
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

	/// A retarget plans the shown plan's own payload: the budget counts that
	/// one allocation once, and the result is the same allocation (#238).
	#[test]
	fn retarget_shares_the_payload_with_the_shown_plan() {
		let dir = tempfile::tempdir().unwrap();
		let other = tempfile::tempdir().unwrap();
		let raw = "// FILE: a.txt\nbody";
		let mut shell = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		shell.disarm_for(other.path().into());
		let pool = Arc::new(Mutex::new(PastePending::default()));
		lock_pending(&pool)
			.admit_ui(None, Some(&shell), None)
			.unwrap();
		let retarget = || PasteRequest::Retarget {
			dest: other.path().into(),
			roots: Vec::new(),
			generation: crate::gen::Stamp::for_tests(2),
		};
		// Nothing to plan from: no job.
		lock_pending(&pool).enqueue(retarget()).unwrap();
		assert!(PastePending::start(&pool, None, CancelToken::new())
			.unwrap()
			.is_none());
		lock_pending(&pool).enqueue(retarget()).unwrap();
		let work = PastePending::start(&pool, Some(&shell), CancelToken::new())
			.unwrap()
			.unwrap();
		{
			let state = lock_pending(&pool);
			assert_eq!(
				state.shared_raw(state.ui_raw.as_ref()),
				raw.len() + ARC_ALLOWANCE,
				"the payload is charged once while the job holds it"
			);
		}
		work.run(&RunOptions::default());
		let outcome = lock_pending(&pool).ready.take().unwrap();
		assert!(outcome.retargeted);
		let plan = outcome.result.unwrap();
		assert!(Arc::ptr_eq(&plan.raw_payload, &shell.raw_payload));
		assert_eq!(plan.destination, other.path());
		assert!(plan.executable());
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		shell.clear_file_plan();
		let pool = Arc::new(Mutex::new(PastePending::default()));
		lock_pending(&pool)
			.admit_ui(None, Some(&shell), None)
			.unwrap();
		lock_pending(&pool)
			.enqueue(PasteRequest::Remap(Remap::Prefix {
				prefix: "a".into(),
				keep: true,
			}))
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
				lock_pending(&pool).invalidate(crate::gen::Stamp::for_tests(1));
			}
			work.run(&RunOptions::default());
			assert!(lock_pending(&pool).input.is_none());
			assert_eq!(lock_pending(&pool).ready.is_none(), invalidate_before);
			lock_pending(&pool).invalidate(crate::gen::Stamp::for_tests(2));
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
				state.ui_commit.as_ref(),
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
					state.ui_raw.as_ref(),
					state.ui_commit.as_ref()
				),
				MAX_RETAINED_PREVIEW_BYTES
			);
			state.admit_ui(Some(&ordinary), None, None).unwrap();
			state.ready = Some(PasteOutcome {
				result: Ok(plan),
				remap: None,
				retargeted: false,
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
					state.ui_raw.as_ref(),
					state.ui_commit.as_ref()
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
				crate::gen::Stamp::for_tests(1),
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
				crate::gen::Stamp::for_tests(1),
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
			crate::gen::Stamp::for_tests(1),
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
			&raw,
			&roots[0],
			&roots,
			crate::gen::Stamp::for_tests(1),
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
				crate::gen::Stamp::for_tests(1),
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
			crate::gen::Stamp::for_tests(1),
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
			crate::gen::Stamp::for_tests(1),
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let limit = full.retained_bytes() - 1;
		assert!(raw.len() < limit);
		assert_eq!(
			PastePreviewPlan::build_with_limit(
				&raw,
				dir.path(),
				&[],
				crate::gen::Stamp::for_tests(1),
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
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
			crate::gen::Stamp::for_tests(1),
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();

		// Destination changes on disk after preview was generated!
		std::thread::sleep(std::time::Duration::from_millis(15));
		fs::write(
			&existing_file,
			"external modification between preview and apply",
		)
		.unwrap();

		// Execute must fail with a meaningful stale destination error!
		let err = plan.execute().unwrap_err().msg;
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
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dest,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
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
		let plan2 = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dest,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		assert_eq!(plan.execute().unwrap_err().msg.key, "mapping_required");
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		(dir, plan)
	}

	#[test]
	fn commit_preview_lists_skips_renames_and_groups_in_path_order() {
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
				// The rename's two rows share the folder of the new path.
				PasteNode::Dir { name, .. } => format!("D {name}"),
				_ => "?".into(),
			})
			.collect();
		assert_eq!(
			shown,
			[
				"C0",
				"D dir",
				"DELETE old.txt",
				"CREATE dir/new.txt",
				"CREATE fresh.txt",
				"SKIP img.bin",
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
			plan.execute().unwrap_err().msg.key,
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		assert_eq!(plan.execute().unwrap_err().msg.key, "mapping_required");
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
			crate::gen::Stamp::for_tests(1),
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
			crate::gen::Stamp::for_tests(1),
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
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
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
		// Rows follow natural path order, so the blocked file is found by path.
		let blocked = plan
			.items
			.iter()
			.find(|i| i.path == "newdir/x.txt")
			.expect("the blocked file has a row");
		assert_eq!(
			blocked.op,
			PlannedOp::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(
			blocked.action(),
			RowAction::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert!(!blocked.action().is_not_written());

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
		let err = plan.execute().unwrap_err().msg;
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
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
		let err = plan.execute().unwrap_err().msg;
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

	/// Makes the worktree's lock file unopenable (a directory stands in
	/// its place), so a replay fails to start without anyone holding it.
	struct HeavyLockBlocker {
		path: PathBuf,
	}

	impl HeavyLockBlocker {
		fn fill(repo: &Path) -> Self {
			use snip_core::gitrun::RunOptions;
			use snip_core::gitsrc::Git;
			use snip_core::workspace::{RepoIdentity, HEAVY_LOCK_FILE};

			let git = Git::open(repo).unwrap();
			let id =
				RepoIdentity::resolve(&git, &RunOptions::default()).unwrap();
			let path = id.git_dir.join(HEAVY_LOCK_FILE);
			fs::create_dir(&path).unwrap();
			Self { path }
		}

		fn release(self) {
			fs::remove_dir(&self.path).unwrap();
		}
	}

	#[test]
	fn a_held_repository_lock_refuses_both_paste_kinds_with_the_busy_message() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		use snip_core::gitsrc::Git;
		use snip_core::workspace::{lock_heavy, RepoIdentity};

		let dir = tempfile::tempdir().unwrap();
		let repo = dunce::canonicalize(dir.path()).unwrap().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);
		let id = RepoIdentity::resolve(
			&Git::open(&repo).unwrap(),
			&RunOptions::default(),
		)
		.unwrap();
		let held = lock_heavy(&id).unwrap();

		let files = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: new.txt\nnew content\n",
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let err = files.execute().unwrap_err().msg;
		assert_eq!(err.key, "paste_repo_busy");
		assert!(!repo.join("new.txt").exists());

		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "first commit\n".into(),
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
		let replay = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let err = replay.execute().unwrap_err().msg;
		assert_eq!(err.key, "paste_repo_busy");
		assert_eq!(
			err.render(crate::i18n::Locale::ZhTw),
			"這個儲存庫正在被另一個貼上作業使用，請稍後再試"
		);
		assert!(!repo.join("b.txt").exists());

		drop(held);
		assert_eq!(files.execute().unwrap().files.created_count, 1);
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();

		let blocker = HeavyLockBlocker::fill(&repo);
		let err = plan.execute().unwrap_err().msg;
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();

		let blocker = HeavyLockBlocker::fill(&repo);
		let err = plan.execute().unwrap_err().msg;
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
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

		let err = plan.execute().unwrap_err().msg;
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
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
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert!(plan.prefix_choices.is_empty());
		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 1);
		assert_eq!(fs::read_to_string(dest.join("lib/a.txt")).unwrap(), "body");
		assert!(!trap.join("a.txt").exists());
	}

	/// A landing that brings the repo set the plan already saw does no work
	/// (#215); a preview opened before the scan keeps the workspace root as
	/// its primary destination (accepted) and only gains candidates.
	#[test]
	fn add_candidates_skips_an_unchanged_repo_set() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("ws");
		let repo = tmp.path().join("ws").join("repo01");
		fs::create_dir_all(&repo).unwrap();
		let payload = "// FILE: repo01/a.txt\nbody\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&dest,
			std::slice::from_ref(&dest),
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let primary = plan.destination.clone();
		let before = plan.prefix_choices[0].candidates.len();
		// Same set as at build: nothing to do, even if a candidate is gone.
		plan.prefix_choices[0].candidates.clear();
		assert!(!plan.add_candidates(std::slice::from_ref(&dest)));
		assert!(plan.prefix_choices[0].candidates.is_empty());
		// The scan found a repo: it is added once, then the set is stale.
		assert!(plan.add_candidates(&[dest.clone(), repo.clone()]));
		assert!(plan.prefix_choices[0].candidates.len() <= before + 2);
		plan.prefix_choices[0].candidates.clear();
		assert!(!plan.add_candidates(&[dest.clone(), repo.clone()]));
		assert!(plan.prefix_choices[0].candidates.is_empty());
		assert_eq!(plan.destination, primary, "the primary destination stays");
		// No mapping to extend: never any work.
		plan.prefix_choices.clear();
		assert!(!plan.add_candidates(&[repo]));
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
			crate::gen::Stamp::for_tests(1),
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
	fn commit_rows_ask_to_overwrite_exactly_the_existing_writes() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);
		let text = commit_payload_modifying_a();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&path,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();

		let asked: BTreeSet<&str> = plan
			.items
			.iter()
			.filter(|i| i.overwritable())
			.map(|i| i.path.as_str())
			.collect();
		let existing = plan
			.commit_preview
			.as_ref()
			.unwrap()
			.plan()
			.existing_writes();
		assert!(!existing.is_empty());
		assert_eq!(asked, existing);
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

		let mut unchecked = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&path,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert!(unchecked.items[0].content.contains("incoming"));
		assert!(!unchecked.items[0].content.contains("Author"));
		assert_eq!(
			unchecked.commit_preview.as_ref().unwrap().plan().commits[0]
				.author_name,
			"Author"
		);
		assert!(!unchecked.items[0].overwrite_allowed);
		unchecked.items[0].selected = false;
		unchecked.items[0].overwrite_allowed = false;
		assert!(unchecked.check_stale_destination().is_ok());
		assert_eq!(
			unchecked.execute().unwrap_err().msg.key,
			"commit_subset_rejected"
		);
		assert_eq!(fs::read_to_string(path.join("a.txt")).unwrap(), "base\n");

		let mut stale = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&path,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
		.unwrap();
		stale.items[0].overwrite_allowed = true;
		fs::write(path.join("a.txt"), "external change\n").unwrap();
		assert_eq!(
			stale.check_stale_destination().unwrap_err().key,
			"stale_modified"
		);
		assert_eq!(stale.execute().unwrap_err().msg.key, "stale_modified");
		assert_eq!(
			fs::read_to_string(path.join("a.txt")).unwrap(),
			"external change\n"
		);
	}

	/// `PASTE_STALE_DETECTED` prints these tokens, and e2e drivers read them:
	/// the table is the contract, whatever the dictionary names its keys.
	#[test]
	fn apply_tokens_are_the_e2e_contract() {
		fn index(t: ApplyToken) -> usize {
			match t {
				ApplyToken::StaleCreated => 0,
				ApplyToken::StaleDeleted => 1,
				ApplyToken::StaleModified => 2,
				ApplyToken::PasteRepoBusy => 3,
				ApplyToken::PasteOutcomeUnknown => 4,
				ApplyToken::PasteErrDestination => 5,
				ApplyToken::PasteErrPlan => 6,
				ApplyToken::ErrorOpenRepo => 7,
				ApplyToken::CommitReplayRefused => 8,
				ApplyToken::CommitReplayPartial => 9,
				ApplyToken::CommitReplayPartialRefused => 10,
				ApplyToken::CommitSubsetRejected => 11,
				ApplyToken::CommitOverwriteRequired => 12,
				ApplyToken::MappingRequired => 13,
			}
		}
		let table = [
			(ApplyToken::StaleCreated, "stale_created"),
			(ApplyToken::StaleDeleted, "stale_deleted"),
			(ApplyToken::StaleModified, "stale_modified"),
			(ApplyToken::PasteRepoBusy, "paste_repo_busy"),
			(ApplyToken::PasteOutcomeUnknown, "paste_outcome_unknown"),
			(ApplyToken::PasteErrDestination, "paste_err_destination"),
			(ApplyToken::PasteErrPlan, "paste_err_plan"),
			(ApplyToken::ErrorOpenRepo, "error_open_repo"),
			(ApplyToken::CommitReplayRefused, "commit_replay_refused"),
			(ApplyToken::CommitReplayPartial, "commit_replay_partial"),
			(
				ApplyToken::CommitReplayPartialRefused,
				"commit_replay_partial_refused",
			),
			(ApplyToken::CommitSubsetRejected, "commit_subset_rejected"),
			(
				ApplyToken::CommitOverwriteRequired,
				"commit_overwrite_required",
			),
			(ApplyToken::MappingRequired, "mapping_required"),
		];
		let seen: BTreeSet<_> = table.iter().map(|(t, _)| index(*t)).collect();
		assert_eq!(seen.len(), 14, "a token has no row");
		for (token, text) in table {
			assert_eq!(token.as_str(), text);
		}
		// The rule of a reason's own token, as it read off the message.
		for (label, reason) in super::zh_leaks_tests::reason_samples() {
			let want = match reason_msg(&reason).key {
				"stale_created" => ApplyToken::StaleCreated,
				"stale_deleted" => ApplyToken::StaleDeleted,
				"paste_repo_busy" => ApplyToken::PasteRepoBusy,
				_ => ApplyToken::StaleModified,
			};
			assert_eq!(ApplyToken::of_reason(&reason), want, "{label}");
		}
	}

	/// Every refusal an Apply can end in prints its own token, along the path
	/// that produces it.
	#[test]
	fn every_apply_refusal_prints_its_token() {
		// mapping_required: two repos, no destination chosen.
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().join("repos");
		fs::create_dir(&root).unwrap();
		let (repo_a, repo_b) = (root.join("repo-a"), root.join("repo-b"));
		fs::create_dir(&repo_a).unwrap();
		fs::create_dir(&repo_b).unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: repo-a/a.txt\nA\n// FILE: repo-b/b.txt\nB\n",
			&repo_a,
			&[repo_a.clone(), repo_b.clone()],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::MappingRequired);
		assert_eq!(err.msg.key, "mapping_required");

		// paste_err_plan: a file Apply without a plan.
		let dest = tmp.path().join("dest");
		fs::create_dir(&dest).unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: a.txt\nA\n",
			&dest,
			std::slice::from_ref(&dest),
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		// stale_created: the file appeared after the preview.
		fs::write(dest.join("a.txt"), "outside\n").unwrap();
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::StaleCreated, "{:?}", err.msg);
		fs::remove_file(dest.join("a.txt")).unwrap();
		plan.import_plan = None;
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::PasteErrPlan);
		assert_eq!(err.msg.key, "paste_err_plan");

		// commit_overwrite_required and commit_subset_rejected.
		let repo = tmp.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);
		let text = commit_payload_modifying_a();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::CommitOverwriteRequired);
		plan.items[0].selected = false;
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::CommitSubsetRejected);
		// stale_modified: the file changed after the preview.
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&text,
			&repo,
			&[],
			crate::gen::Stamp::for_tests(2),
		)
		.unwrap();
		plan.items[0].overwrite_allowed = true;
		fs::write(repo.join("a.txt"), "external change\n").unwrap();
		let err = plan.execute().unwrap_err();
		assert_eq!(err.token, ApplyToken::StaleModified);

		// The rest, from the errors the paste engine gives them.
		let lost = || {
			RemoteError::Io(std::io::Error::from(
				std::io::ErrorKind::BrokenPipe,
			))
		};
		let busy = || {
			PasteError::Local(TransferError::Git(GitError::WorktreeBusy {
				args: "x".into(),
			}))
		};
		let missing = || PasteError::Destination {
			path: dest.clone(),
			source: std::io::Error::from(std::io::ErrorKind::NotFound),
		};
		let stale = |change| {
			PasteError::Remote(RemoteError::Refused {
				code: snip_remote::ErrorCode::Stale,
				message: "english".into(),
				reason: Some(Box::new(Reason::DestinationChanged { change })),
				suggestion: None,
			})
		};
		let file = |e| file_apply_refusal(None, &dest, e).token;
		assert_eq!(file(busy()), ApplyToken::PasteRepoBusy);
		assert_eq!(
			file(PasteError::OutcomeUnknown(lost())),
			ApplyToken::PasteOutcomeUnknown
		);
		assert_eq!(file(missing()), ApplyToken::PasteErrDestination);
		assert_eq!(
			file(stale(StaleChange::Deleted { rel: "a".into() })),
			ApplyToken::StaleDeleted
		);
		assert_eq!(
			file(stale(StaleChange::Created { rel: "a".into() })),
			ApplyToken::StaleCreated
		);
		let commit =
			|e, applying| commit_refusal(None, &dest, e, applying).token;
		assert_eq!(commit(busy(), true), ApplyToken::PasteRepoBusy);
		assert_eq!(
			commit(
				PasteError::Local(TransferError::Git(GitError::NotFound)),
				true
			),
			ApplyToken::ErrorOpenRepo
		);
		assert_eq!(
			commit(
				PasteError::Remote(RemoteError::Refused {
					code: snip_remote::ErrorCode::Busy,
					message: "english".into(),
					reason: None,
					suggestion: None,
				}),
				true
			),
			ApplyToken::ErrorOpenRepo
		);
		assert_eq!(
			commit(PasteError::OutcomeUnknown(lost()), true),
			ApplyToken::PasteOutcomeUnknown
		);
		assert_eq!(commit(missing(), true), ApplyToken::PasteErrDestination);
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert_eq!(plan.items[0].op, PlannedOp::Skip(SkipCause::NonUtf8Target));
		assert!(!plan.items[0].overwrite_allowed);
		fs::write(path.join("a.txt"), "external change\n").unwrap();
		assert_eq!(
			plan.check_stale_destination().unwrap_err().key,
			"stale_modified"
		);
		assert_eq!(plan.execute().unwrap_err().msg.key, "stale_modified");
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		assert_eq!(
			plan.execute().unwrap_err().msg.key,
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
			crate::gen::Stamp::for_tests(2),
		)
		.unwrap();
		let err = nested_plan.execute().unwrap_err().msg;
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
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();

		fs::write(path.join("a.txt"), "external\n").unwrap();
		let err = plan.execute().unwrap_err().msg;
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
			crate::gen::Stamp::for_tests(1),
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
				PasteNode::Dir { name, .. } => format!("D {name}"),
				_ => "?".into(),
			})
			.collect();
		assert_eq!(
			shape,
			[
				"C0",
				"D dir",
				"Delete:old.txt",
				"Create:dir/new.txt",
				"Create:fresh.txt",
				"Skip(Binary):img.bin",
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
		let mut excluded = plan
			.items
			.iter()
			.find(|i| i.path == "fresh.txt")
			.unwrap()
			.clone();
		excluded.selected = false;
		assert_eq!(excluded.action(), RowAction::Excluded { by_commit: true });
		assert!(plan.executable());
	}

	#[test]
	fn up_down_walk_commit_rows_in_path_order() {
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
			["old.txt", "dir/new.txt", "fresh.txt", "img.bin", "a.txt"]
		);
		// A folded commit's rows are skipped, and Up re-enters
		// from a selection that is folded away.
		plan.toggle_commit_collapsed(0);
		plan.select_prev();
		assert_eq!(plan.items[plan.selected_item_idx].path, "a.txt");
	}

	#[test]
	fn commit_rows_follow_natural_path_order() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some(format!("{path}\n")),
			not_copied: None,
		};
		let payload = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "natural\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![
					file("g1.txt"),
					file("g10.txt"),
					file("g11.txt"),
					file("g2.txt"),
				],
			}],
		});
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		for args in [&["add", "."][..], &["commit", "-qm", "base"][..]] {
			git_run(&path, args);
		}
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&payload,
			&path,
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let shown: Vec<String> = plan
			.commit_rows()
			.iter()
			.filter_map(|r| match r {
				PasteNode::File(ix, _) => Some(plan.items[*ix].path.clone()),
				_ => None,
			})
			.collect();
		assert_eq!(shown, ["g1.txt", "g2.txt", "g10.txt", "g11.txt"]);
		assert_eq!(plan.items[0].path, "g1.txt");
	}

	/// Two commits that both touch `src/`, one with a file outside it.
	fn shared_folder_plan() -> (tempfile::TempDir, PastePreviewPlan) {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let added = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some(format!("{path}\n")),
			not_copied: None,
		};
		let record = |message: &str, files| CommitRecord {
			message: message.into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files,
		};
		let payload = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				record(
					"first",
					vec![
						added("src/a.txt"),
						added("src/b.txt"),
						added("top.txt"),
					],
				),
				record("second", vec![added("src/c.txt")]),
			],
		});
		let dir = tempfile::tempdir().unwrap();
		git_init(dir.path());
		fs::write(dir.path().join("base.txt"), "base\n").unwrap();
		for args in [&["add", "."][..], &["commit", "-qm", "base"][..]] {
			git_run(dir.path(), args);
		}
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&payload,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		(dir, plan)
	}

	/// The folder `path` under commit `c` of a replay plan.
	fn commit_dir(plan: &PastePreviewPlan, c: usize, path: &str) -> DirKey {
		DirKey {
			commit: Some(c),
			root: plan.items[0].dest_root.clone(),
			path: path.into(),
		}
	}

	#[test]
	fn commit_rows_nest_folders_and_a_rename_stays_under_its_new_path() {
		let (_dir, plan) = mixed_plan();
		assert_eq!(
			shape(&plan),
			[
				"C 0",
				"D dir 2 d1",
				"F old.txt d2",
				"F dir/new.txt d2",
				"F fresh.txt d1",
				"F img.bin d1",
				"C 1",
				"C 2",
				"F a.txt d1",
			]
		);
		// The deleted old path is placed where the new one is (#196).
		let old = ix_of(&plan, "old.txt");
		assert_eq!(plan.place_path(old), "dir/new.txt");
		assert_eq!(plan.place_path(ix_of(&plan, "fresh.txt")), "fresh.txt");
		let dir = commit_dir(&plan, 0, "dir");
		assert_eq!(
			plan.counts_under(&dir),
			PasteCounts {
				creates: 1,
				deletes: 1,
				..Default::default()
			}
		);
		// What the folder row shows: both rows written, none toggleable.
		let sum = plan.summary(&[old, ix_of(&plan, "dir/new.txt")]);
		assert_eq!((sum.files, sum.included), (2, 2));
		assert_eq!(sum.tick(), Tick::Locked);
		assert_eq!(plan.dir_label(&dir), "0:dir");
	}

	#[test]
	fn commit_folds_are_keyed_by_commit() {
		let (_dir, mut plan) = shared_folder_plan();
		assert_eq!(
			shape(&plan),
			[
				"C 0",
				"D src 2 d1",
				"F src/a.txt d2",
				"F src/b.txt d2",
				"F top.txt d1",
				"C 1",
				"D src 1 d1",
				"F src/c.txt d2",
			]
		);
		let (a, top, c) = (
			ix_of(&plan, "src/a.txt"),
			ix_of(&plan, "top.txt"),
			ix_of(&plan, "src/c.txt"),
		);
		let first = commit_dir(&plan, 0, "src");
		assert_eq!(plan.dir_label(&commit_dir(&plan, 1, "src")), "1:src");
		assert!(plan.has_dir_row(&first));
		assert!(!plan.has_dir_row(&commit_dir(&plan, 2, "src")));

		// Folding the first commit's `src` while its `a.txt` is selected
		// names the row to move to; the cursor itself stays for the session.
		plan.selected_item_idx = a;
		assert_eq!(
			plan.toggle_dir_collapsed(&first),
			Some(PasteCursor::Item(top))
		);
		assert_eq!(plan.selected_item_idx, a);
		assert_eq!(
			shape(&plan),
			[
				"C 0",
				"D src 2 d1+",
				"F top.txt d1",
				"C 1",
				"D src 1 d1",
				"F src/c.txt d2",
			]
		);
		assert!(plan.display_order().contains(&c), "the other commit's");
		// A folder under a folded header is not a row now.
		plan.toggle_commit_collapsed(0);
		assert!(!plan.has_dir_row(&first));
		// Opening it again moves nothing.
		plan.toggle_commit_collapsed(0);
		plan.selected_item_idx = top;
		assert_eq!(plan.toggle_dir_collapsed(&first), None);
	}

	#[test]
	fn a_replays_folders_are_not_cursor_rows_and_cannot_be_toggled() {
		let (_dir, mut plan) = shared_folder_plan();
		assert!(plan
			.cursor_rows()
			.iter()
			.all(|(c, ..)| matches!(c, PasteCursor::Item(_))));
		let first = commit_dir(&plan, 0, "src");
		assert_eq!(plan.nav_left(), TreeStep::Nothing);
		assert_eq!(plan.nav_right(), TreeStep::Nothing);
		assert_eq!(plan.set_dir_selected(&first), None);
		assert!(plan.items.iter().all(|i| i.selected));
		// A row left out of the replay is not counted as written.
		plan.items[0].selected = false;
		let sum = plan.summary(&[0, 1]);
		assert_eq!((sum.files, sum.included), (2, 1));
	}

	#[test]
	fn file_mode_rows_follow_natural_path_order() {
		let dir = tempfile::tempdir().unwrap();
		let payload =
			"// FILE: g10.txt\na\n// FILE: g2.txt\nb\n// FILE: g1.txt\nc\n";
		let plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dir.path(),
			&[],
			crate::gen::Stamp::for_tests(1),
		)
		.unwrap();
		let order: Vec<String> = plan
			.display_order()
			.into_iter()
			.map(|ix| plan.items[ix].path.clone())
			.collect();
		assert_eq!(order, ["g1.txt", "g2.txt", "g10.txt"]);
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
			crate::gen::Stamp::for_tests(1),
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
		assert_eq!(target, PasteCursor::Item(1));
		let target_back = plan.step_target(false);
		assert_eq!(
			plan.selected_item_idx, 0,
			"step_target must not mutate selection"
		);
		assert_eq!(target_back, PasteCursor::Item(0));

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

#[cfg(test)]
thread_local! {
	/// Choices `candidate_growth_bytes` looked at on this thread: a test
	/// counts work with it instead of timing it.
	pub(crate) static GROWTH_CHOICES_VISITED: std::cell::Cell<usize> =
		const { std::cell::Cell::new(0) };
}
