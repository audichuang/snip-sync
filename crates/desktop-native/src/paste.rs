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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use snip_core::commits::{self, ReplayAction};
use snip_core::format;
use snip_core::gitrun::RunOptions;
use snip_core::gitsrc::Git;
use snip_core::restore::{
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	detect_clipboard_prefixes, plan_import_with, CanonicalRootId,
	CommitReplayPreview, ImportMapping, TransferError, TransferImportPlan,
};

use crate::i18n::Msg;

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

/// Result of a confirmed apply. `created_commits` is empty for file restores.
#[derive(Debug, Clone)]
pub struct PasteApplyResult {
	pub files: RestoreExecutionResult,
	pub created_commits: Vec<String>,
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
		if commits::is_commit_payload(raw_text) {
			return Self::build_commit(raw_text, dest, generation, opts);
		}
		let entries = format::parse_clipboard(raw_text, "");
		if entries.is_empty() {
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
			prefix_choices: prefixes
				.into_iter()
				.map(|prefix| PrefixChoice {
					prefix,
					destination: None,
					keep_relative: false,
					candidates: candidates.clone(),
				})
				.collect(),
			plan: empty_plan(),
			items: Vec::new(),
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw_text.into(),
		};
		plan.rebuild_file_plan_with(opts)?;
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
	) -> Result<Self, Msg> {
		let payload = commits::parse_commit_payload(raw_text)
			.map_err(|e| Msg::new("paste_err_plan", [e.to_string()]))?;
		let preview = CommitReplayPreview::capture_with(dest, &payload, opts)
			.map_err(|e| {
			Msg::new("paste_err_destination", [e.to_string()])
		})?;
		let dest_name = root_name(dest);
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
				let body = src.content.clone().unwrap_or_default();
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
				items.push(PasteItem {
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
				});
			}
		}
		Ok(Self {
			destination: preview.destination.clone(),
			import_plan: None,
			commit_preview: Some(Arc::new(preview)),
			whole_commit: true,
			prefix_choices: Vec::new(),
			plan: empty_plan(),
			items,
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw_text.into(),
		})
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
		if self.whole_commit {
			return Ok(());
		}
		self.clear_file_plan();
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
				self.error = Some(Msg::new("paste_err_plan", [e.to_string()]));
				return Ok(());
			}
		};
		let planned = import_plan.restore_plan().clone();
		if planned.create_operations.is_empty()
			&& planned.delete_operations.is_empty()
			&& planned.skipped_operations.is_empty()
		{
			self.error = Some(Msg::new("paste_err_nothing", []));
			return Ok(());
		}
		self.fill_items(&planned, &dest_roots);
		self.plan = Arc::new(planned);
		self.import_plan = Some(Arc::new(import_plan));
		self.error = None;
		Ok(())
	}

	fn fill_items(&mut self, plan: &RestorePlan, dest_roots: &[PathBuf]) {
		let mut items = Vec::new();
		for (op_idx, op) in plan.create_operations.iter().enumerate() {
			let content: Arc<str> = op.content.as_str().into();
			let bytes = content.len();
			let lines = content.lines().count();
			let dest_root = op.root_path.clone();
			items.push(PasteItem {
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
			});
		}
		for (op_idx, op) in plan.delete_operations.iter().enumerate() {
			let dest_root = dest_roots
				.iter()
				.find(|r| op.absolute_path.starts_with(r))
				.cloned()
				.unwrap_or_else(|| self.destination.clone());
			items.push(PasteItem {
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
			});
		}
		self.items = items;
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

	pub fn select_prev(&mut self) {
		if self.selected_item_idx > 0 {
			self.selected_item_idx -= 1;
		}
	}

	pub fn select_next(&mut self) {
		if self.selected_item_idx + 1 < self.items.len() {
			self.selected_item_idx += 1;
		}
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
	use std::fs;

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
		let repo_a = tmp.path().join("repo-a");
		let repo_b = tmp.path().join("repo-b");
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
		assert_eq!(item_a.dest_root, repo_a);
		let item_b =
			plan.items.iter().find(|i| i.path == "file_b.txt").unwrap();
		assert_eq!(item_b.dest_root, repo_b);

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
