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

use snip_core::commits::{self, ReplayAction};
use snip_core::format;
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::Git;
use snip_core::restore::{
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	detect_clipboard_prefixes, plan_import_with, CanonicalRootId,
	CommitReplayPreview, ImportMapping, TransferError, TransferImportPlan,
};

use crate::i18n::Msg;
use crate::reader::{Preview, PreviewSource};
use crate::syntax::Language;

/// Shared retained preview tier: UI, one active paste input, one latest
/// captured request, and one result mailbox. Builder scratch is temporary.
pub const MAX_RETAINED_PREVIEW_BYTES: usize = 32 * 1024 * 1024;
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

#[derive(Debug, Clone)]
pub struct PasteItem {
	pub path: String,
	pub dest_root: PathBuf,
	pub dest_root_name: String,
	pub dest_path: PathBuf,
	pub dest_exists: bool,
	pub is_delete: bool,
	pub action_label: &'static str,
	pub overwrite_allowed: bool, // CRITICAL: OFF by default!
	pub selected: bool,          // CRITICAL: whether this item is opted-in
	/// Shared with the plan's detail view; never copied per render.
	pub content: Arc<str>,
	pub bytes: usize,
	pub lines: usize,
	pub op_index: usize,
}

impl PasteItem {
	fn retained_heap_bytes(&self) -> usize {
		self.path
			.capacity()
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

fn root_name(path: &Path) -> String {
	path.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| path.display().to_string())
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

impl PastePreviewPlan {
	pub fn detail_preview(&self) -> Option<Preview> {
		self.detail_at(self.selected_item_idx)
	}

	pub(crate) fn detail_at(&self, idx: usize) -> Option<Preview> {
		self.items.get(idx).filter(|i| !i.is_delete).map(|i| {
			Preview::new(
				PreviewSource::PasteItem,
				Some(i.path.clone()),
				i.content.to_string(),
				false,
				Language::from_path_or_ext(&i.path, false),
			)
		})
	}

	/// Read-only navigation cannot revoke an admitted plan or change its
	/// selection/busy state on refusal. Keep the previous matching detail.
	pub fn select_detail(
		&mut self,
		idx: usize,
		ordinary: Option<&Preview>,
		detail: &mut Option<Preview>,
	) -> Result<(), Msg> {
		self.select_detail_with_limit(
			idx,
			ordinary,
			detail,
			MAX_RETAINED_PREVIEW_BYTES,
		)
	}

	fn select_detail_with_limit(
		&mut self,
		idx: usize,
		ordinary: Option<&Preview>,
		detail: &mut Option<Preview>,
		limit: usize,
	) -> Result<(), Msg> {
		if idx >= self.items.len() {
			return Err(Msg::new("paste_err_nothing", []));
		}
		let candidate = self.detail_at(idx);
		admit_preview_state_with_limit(
			ordinary,
			Some(self),
			candidate.as_ref(),
			limit,
		)?;
		self.selected_item_idx = idx;
		*detail = candidate;
		Ok(())
	}

	/// Consume a candidate so rejection cannot return an armed partial plan.
	/// Ordinary reader and selected detail coexist and share the same cap.
	pub fn admit_with_preview(
		self,
		ordinary: Option<&Preview>,
	) -> Result<(Self, Option<Preview>), Msg> {
		self.admit_with_preview_limit(ordinary, MAX_RETAINED_PREVIEW_BYTES)
	}

	fn admit_with_preview_limit(
		self,
		ordinary: Option<&Preview>,
		limit: usize,
	) -> Result<(Self, Option<Preview>), Msg> {
		let detail = self.detail_preview();
		admit_preview_state_with_limit(
			ordinary,
			Some(&self),
			detail.as_ref(),
			limit,
		)?;
		Ok((self, detail))
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
			.map_err(|e| {
			Msg::new("paste_err_destination", [e.to_string()])
		})?;
		let preview = Arc::new(preview);
		let mut plan = Self {
			destination: preview.destination.clone(),
			import_plan: None,
			commit_preview: Some(preview.clone()),
			whole_commit: true,
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
			.replay
			.commits
			.iter()
			.zip(&preview.payload.commits)
			.enumerate()
		{
			for (f_idx, (file, src)) in
				commit.files.iter().zip(&record.files).enumerate()
			{
				let action_label = match file.action {
					ReplayAction::Delete => "DELETE",
					ReplayAction::Write if file.existed => "OVERWRITE",
					ReplayAction::Write => "CREATE",
					ReplayAction::Skip => "SKIP",
				};
				let body = src.content.as_deref().unwrap_or_default();
				let text = format!(
					"commit: {}\nauthor: {} <{}>\ndate: {}\naction: {action_label}\npath: {}\n\n{body}",
					commit.message.trim_end(),
					commit.author_name,
					commit.author_email,
					commit.author_date,
					file.path,
				);
				let content: Arc<str> = Arc::from(text);
				let dest_path = file
					.absolute_path
					.clone()
					.unwrap_or_else(|| dest.join(&file.path));
				let item = PasteItem {
					path: file.path.clone(),
					dest_root: dest.to_path_buf(),
					dest_root_name: dest_name.clone(),
					dest_path,
					dest_exists: file.existed,
					is_delete: file.action == ReplayAction::Delete,
					action_label,
					overwrite_allowed: false,
					selected: true,
					content,
					bytes: body.len(),
					lines: body.lines().count(),
					op_index: c_idx * 1000 + f_idx,
				};
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
		plan.items = items;
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
			.map_err(|e| Msg::new("paste_err_destination", [e.to_string()]))?;
		if !id.path().is_dir() {
			return Err(Msg::new(
				"paste_err_destination",
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
			.map_err(|e| Msg::new("paste_err_destination", [e.to_string()]))?;
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
			let id = CanonicalRootId::new(&dest).map_err(|e| {
				Msg::new("paste_err_destination", [e.to_string()])
			})?;
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
				is_delete: false,
				action_label: if op.existed { "OVERWRITE" } else { "CREATE" },
				overwrite_allowed: false,
				selected: true,
				content,
				bytes,
				lines,
				op_index: op_idx,
				dest_root,
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
				is_delete: true,
				action_label: "DELETE",
				overwrite_allowed: true,
				selected: true,
				content: Arc::from(""),
				bytes: 0,
				lines: 0,
				op_index: op_idx,
				dest_root,
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

	pub fn toggle_selected(&mut self, idx: usize) {
		if let Some(item) = self.items.get_mut(idx) {
			item.selected = !item.selected;
		}
		if self.whole_commit && self.items.iter().any(|i| !i.selected) {
			self.error = Some(Msg::new("commit_subset_rejected", []));
		}
	}

	/// Up / Down follow the rows on screen (the change tree's order).
	pub fn select_prev(&mut self) {
		self.step_selection(false);
	}

	pub fn select_next(&mut self) {
		self.step_selection(true);
	}

	fn step_selection(&mut self, forward: bool) {
		let order = self.display_order();
		let Some(pos) = order.iter().position(|&i| i == self.selected_item_idx)
		else {
			return;
		};
		let next = if forward {
			pos.checked_add(1)
		} else {
			pos.checked_sub(1)
		};
		if let Some(&ix) = next.and_then(|n| order.get(n)) {
			self.selected_item_idx = ix;
		}
	}

	/// Item indices in change-tree order: by destination root, then
	/// directory (a root's own files first), then name. `items` itself keeps
	/// the plan's order.
	pub fn display_order(&self) -> Vec<usize> {
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

	pub fn set_all_overwrite(&mut self, allowed: bool) {
		for item in &mut self.items {
			if item.dest_exists && !item.is_delete {
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
				if item.is_delete {
					unchecked_deletes.insert(item.op_index);
				} else {
					unchecked_creates.insert(item.op_index);
				}
				continue;
			}
			if item.is_delete {
				// selected delete proceeds
			} else if item.dest_exists && !item.overwrite_allowed {
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
		preview.revalidate().map_err(stale_msg)?;
		let overwrite_missing = self.items.iter().any(|item| {
			item.dest_exists
				&& !item.is_delete
				&& item.action_label != "SKIP"
				&& !item.overwrite_allowed
		});
		if overwrite_missing {
			return Err(Msg::new("commit_overwrite_required", []));
		}
		let git = Git::open(&self.destination)
			.map_err(|e| Msg::new("error_open_repo", [e.to_string()]))?;
		let replay_res = commits::replay(&git, &preview.payload);
		if let Some(fail) = replay_res.failure {
			let created = if replay_res.created.is_empty() {
				"(none)".to_string()
			} else {
				replay_res.created.join(", ")
			};
			return Err(Msg::new(
				"commit_replay_partial",
				[created, fail.error],
			));
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
mod tests {
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
		let rejected = plan.clone();
		let reject_limit = ordinary.retained_bytes()
			+ rejected.retained_bytes()
			+ detail_bytes
			- 1;
		assert!(rejected
			.admit_with_preview_limit(Some(&ordinary), reject_limit)
			.is_err());
		let (mut plan, mut detail) = plan
			.admit_with_preview_limit(Some(&ordinary), exact)
			.unwrap();
		let prior = detail.as_ref().unwrap().fingerprint();
		let selections: Vec<_> = plan
			.items
			.iter()
			.map(|i| (i.selected, i.overwrite_allowed))
			.collect();
		plan.is_applying = true;
		assert_eq!(
			plan.select_detail_with_limit(
				1,
				Some(&ordinary),
				&mut detail,
				exact
			)
			.unwrap_err()
			.key,
			"preview_memory_limit"
		);
		assert_eq!(plan.selected_item_idx, 0);
		assert!(plan.is_applying);
		assert!(plan.import_plan.is_some());
		assert_eq!(detail.as_ref().unwrap().fingerprint(), prior);
		assert_eq!(detail.as_ref().unwrap().path.as_deref(), Some("small.txt"));
		assert_eq!(
			plan.items
				.iter()
				.map(|i| (i.selected, i.overwrite_allowed))
				.collect::<Vec<_>>(),
			selections
		);
		plan.is_applying = false;
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
		let head = std::process::Command::new("git")
			.current_dir(dir.path())
			.args(["rev-parse", "--verify", "HEAD"])
			.output()
			.unwrap();
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
		assert_eq!(existing.action_label, "OVERWRITE");
		assert!(
			!existing.overwrite_allowed,
			"overwrite must be off by default"
		);

		let new_item = plan.items.iter().find(|i| i.path == "new.txt").unwrap();
		assert!(!new_item.dest_exists);
		assert_eq!(new_item.action_label, "CREATE");

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
		assert!(del_item.is_delete);
		assert_eq!(del_item.action_label, "DELETE");

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

	fn git_init(path: &Path) {
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(path)
				.args(args)
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		git(&["init", "-q", "-b", "main"]);
		git(&["config", "user.name", "Probe"]);
		git(&["config", "user.email", "probe@example.invalid"]);
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
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(&path)
				.args(args)
				.output()
				.unwrap();
			assert!(out.status.success());
		};
		git(&["add", "."]);
		git(&["commit", "-qm", "base"]);
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
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(&path)
				.args(args)
				.output()
				.unwrap();
			assert!(out.status.success(), "{args:?}");
			String::from_utf8(out.stdout).unwrap()
		};
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
		assert_eq!(plan.items[0].action_label, "SKIP");
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
		let git = |args: &[&str]| {
			std::process::Command::new("git")
				.current_dir(&path)
				.args(args)
				.status()
				.unwrap()
				.success()
		};
		assert!(git(&["add", "."]) && git(&["commit", "-qm", "base"]));

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
		let meta = std::process::Command::new("git")
			.current_dir(&path)
			.args(["log", "-1", "--format=%an|%ae|%B"])
			.output()
			.unwrap();
		let meta = String::from_utf8(meta.stdout).unwrap();
		assert!(meta.contains("Author|author@example.invalid|incoming"));
		let body = std::process::Command::new("git")
			.current_dir(&path)
			.args(["show", "HEAD:a.txt"])
			.output()
			.unwrap();
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
		assert_eq!(err.key, "commit_replay_partial");
		assert_ne!(err.args[0], "(none)");
		assert_eq!(
			fs::read_to_string(path.join("leaf")).unwrap(),
			"file-not-dir\n"
		);
		assert!(!path.join("leaf/child.txt").exists());
	}
}
