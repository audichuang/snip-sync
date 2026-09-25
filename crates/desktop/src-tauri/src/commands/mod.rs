//! Thin Tauri commands over `snip-core`. Every piece of logic lives in the
//! core; these only move data between the webview, the clipboard and it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use snip_core::browser::{
	self, CommitSummary, DirectoryEntry, RepositoryHistory, SourcePreview,
};
use snip_core::commits::{
	self, CommitCopySummary, CommitReplayPlan, CommitsPayload, ReplayResult,
};
use snip_core::copy::{collect_copy_files, CopyResult};
use snip_core::format::{extract_source_root, parse_clipboard};
use snip_core::fsutil::read_text_file;
use snip_core::gitsrc::{self, Git, GitSource};
use snip_core::restore::{
	apply_restore_base, execute_restore_plan, plan_restore,
	suggest_restore_base, DirProbe, RestoreBase, RestoreBaseSuggestion,
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::settings::{normalize, Settings};
use snip_core::{clip, clip::Mode};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;
use ts_rs::TS;

type CmdResult<T> = Result<T, String>;

/// What the last paste preview planned. Apply / replay run exactly this,
/// never a re-read clipboard: the user confirmed what they saw.
enum Pending {
	Files(RestorePlan),
	Commits {
		root: PathBuf,
		payload: CommitsPayload,
		plan: CommitReplayPlan,
	},
}

#[derive(Default)]
pub struct AppState {
	/// Re-run by the tray's "copy last selection".
	last_copy: Mutex<Option<LastCopy>>,
	pending: Mutex<Option<Pending>>,
}

#[derive(Clone)]
pub enum LastCopy {
	Payload(CopyRequest),
	Commits(PathBuf, CommitSelection),
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GitSourceDto {
	Working,
	Staged,
	Commit { sha: String },
	Range { base: String, tip: String },
}

impl From<GitSourceDto> for GitSource {
	fn from(s: GitSourceDto) -> Self {
		match s {
			GitSourceDto::Working => Self::Working,
			GitSourceDto::Staged => Self::Staged,
			GitSourceDto::Commit { sha } => Self::Commit(sha),
			GitSourceDto::Range { base, tip } => Self::Range(base, tip),
		}
	}
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CopyRequest {
	/// Files and folders as they are on disk.
	Files {
		roots: Vec<PathBuf>,
		paths: Vec<PathBuf>,
	},
	/// Changes of `repo`; labelled against `roots` (the repo when empty).
	Git {
		repo: PathBuf,
		roots: Vec<PathBuf>,
		source: GitSourceDto,
		selected_paths: Option<Vec<String>>,
	},
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
	pub path: String,
	pub change_type: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GitBrowse {
	pub root: String,
	pub branch: String,
	pub scope: String,
	pub changes: Vec<GitChange>,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CommitSelection {
	/// The last `n` commits on HEAD's first-parent chain.
	Last {
		n: usize,
	},
	From {
		tip: String,
		n: usize,
	},
	/// `base..tip`, base excluded.
	Range {
		base: String,
		tip: String,
	},
}

/// Copy notification numbers (spec 3.1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CopyOutcome {
	pub copied_file_count: usize,
	pub skipped_file_size_count: usize,
	pub skipped_unreadable_count: usize,
	pub file_limit_reached: bool,
	pub file_count_limit: f64,
	pub chars: usize,
	pub lines: usize,
	pub words: usize,
	pub tokens: usize,
}

/// Emitted to the webview after a tray-triggered copy.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum CopyDone {
	Files(CopyOutcome),
	Commits(CommitCopySummary),
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ClipboardPlan {
	Files {
		plan: RestorePlan,
		/// Folder-level offset to offer (single root, no `restoreBase` given);
		/// pass it back as `restoreBase` once the user accepts it.
		suggestion: Option<RestoreBaseSuggestion>,
	},
	Commits {
		plan: CommitReplayPlan,
	},
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DiffTarget {
	/// A create operation of the pending file plan.
	Restore { index: usize },
	/// A file of the pending commit payload.
	Commit { commit: usize, file: usize },
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Settings live in the store under `settings`, in the TS JSON shape.
fn load_settings(app: &AppHandle) -> Settings {
	app.store("settings.json")
		.ok()
		.and_then(|s| s.get("settings"))
		.map(|v| normalize(&v))
		.unwrap_or_default()
}

async fn blocking<T: Send + 'static>(
	f: impl FnOnce() -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
	tauri::async_runtime::spawn_blocking(f)
		.await
		.map_err(|e| e.to_string())?
}

fn outcome(r: &CopyResult, settings: &Settings) -> CopyOutcome {
	let s = r.stats();
	CopyOutcome {
		copied_file_count: r.copied_file_count,
		skipped_file_size_count: r.skipped_file_size_count,
		skipped_unreadable_count: r.skipped_unreadable_count,
		file_limit_reached: r.file_limit_reached,
		file_count_limit: settings.file_count_limit,
		chars: s.chars,
		lines: s.lines,
		words: s.words,
		tokens: s.tokens,
	}
}

fn copy_payload(
	request: &CopyRequest,
	settings: &Settings,
) -> CmdResult<CopyOutcome> {
	let result = match request {
		CopyRequest::Files { roots, paths } => {
			// As TS: nothing selected leaves the clipboard alone.
			if paths.is_empty() {
				return Err("No files selected.".into());
			}
			if roots.is_empty() {
				return Err("No workspace folder found.".into());
			}
			collect_copy_files(roots, paths, settings)
		}
		CopyRequest::Git {
			repo,
			roots,
			source,
			selected_paths,
		} => {
			let git = Git::open(repo).map_err(|e| e.to_string())?;
			let source = source.clone().into();
			let selected = selected_paths
				.as_ref()
				.map(|p| p.iter().cloned().collect::<HashSet<_>>());
			let r = gitsrc::collect_payload_with_selection(
				&git,
				&source,
				roots,
				settings,
				Some(repo),
				selected.as_ref(),
			)
			.map_err(|e| e.to_string())?;
			// As TS: an empty git copy leaves the clipboard alone.
			if r.files.is_empty() {
				return Err("No Git changes found to copy.".into());
			}
			r
		}
	};
	clip::write_text(&result.payload).map_err(|e| e.to_string())?;
	Ok(outcome(&result, settings))
}

#[tauri::command]
pub async fn browse_git(
	repo: PathBuf,
	source: GitSourceDto,
) -> CmdResult<GitBrowse> {
	blocking(move || {
		let git = Git::open(&repo).map_err(|e| e.to_string())?;
		let scope = dunce::canonicalize(&repo).map_err(|e| e.to_string())?;
		let branch = git
			.run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
			.map(|b| String::from_utf8_lossy(&b).trim().to_string())
			.unwrap_or_else(|_| "HEAD".to_string());
		let changes = gitsrc::list_changed_paths(&git, &source.into())
			.map_err(|e| e.to_string())?
			.into_iter()
			.filter(|(path, _)| git.root().join(path).starts_with(&scope))
			.map(|(path, change_type)| GitChange {
				path,
				change_type: change_type
					.map(|c| c.as_str().to_string())
					.unwrap_or_default(),
			})
			.collect();
		Ok(GitBrowse {
			root: git.root().to_string_lossy().into_owned(),
			branch,
			scope: scope
				.strip_prefix(git.root())
				.unwrap_or(Path::new(""))
				.to_string_lossy()
				.into_owned(),
			changes,
		})
	})
	.await
}

fn copy_commit_range(
	repo: &Path,
	selection: &CommitSelection,
) -> CmdResult<CommitCopySummary> {
	let git = Git::open(repo).map_err(|e| e.to_string())?;
	let shas = match selection {
		CommitSelection::Last { n } => commits::select_last(&git, *n),
		CommitSelection::From { tip, n } => {
			commits::select_last_from(&git, tip, *n)
		}
		CommitSelection::Range { base, tip } => {
			commits::select_range(&git, base, tip)
		}
	}
	.map_err(|e| e.to_string())?;
	let payload =
		commits::copy_commits(&git, &shas).map_err(|e| e.to_string())?;
	let text = commits::to_clipboard_text(&payload);
	clip::write_text(&text).map_err(|e| e.to_string())?;
	Ok(commits::copy_summary(&payload, &text))
}

/// Tray entry point: repeats the last copy of either kind.
pub(crate) fn repeat_last_copy(
	app: &AppHandle,
	state: &AppState,
) -> CmdResult<CopyDone> {
	let last = lock(&state.last_copy).clone();
	match last {
		None => Err("Nothing has been copied yet.".into()),
		Some(LastCopy::Payload(req)) => {
			copy_payload(&req, &load_settings(app)).map(CopyDone::Files)
		}
		Some(LastCopy::Commits(repo, sel)) => {
			copy_commit_range(&repo, &sel).map(CopyDone::Commits)
		}
	}
}

#[tauri::command]
pub async fn copy(
	app: AppHandle,
	request: CopyRequest,
) -> CmdResult<CopyOutcome> {
	let settings = load_settings(&app);
	let handle = app.clone();
	blocking(move || {
		let out = copy_payload(&request, &settings)?;
		let state = tauri::Manager::state::<AppState>(&handle);
		*lock(&state.last_copy) = Some(LastCopy::Payload(request));
		Ok(out)
	})
	.await
}

#[tauri::command]
pub async fn copy_commits(
	app: AppHandle,
	repo: PathBuf,
	selection: CommitSelection,
) -> CmdResult<CommitCopySummary> {
	blocking(move || {
		let out = copy_commit_range(&repo, &selection)?;
		let state = tauri::Manager::state::<AppState>(&app);
		*lock(&state.last_copy) = Some(LastCopy::Commits(repo, selection));
		Ok(out)
	})
	.await
}

/// Recent history for the timeline, newest first, all branches' shape
/// left to git's default order.
#[tauri::command]
pub async fn list_commits(
	repo: PathBuf,
	limit: usize,
) -> CmdResult<Vec<CommitSummary>> {
	blocking(move || {
		let git = Git::open(&repo).map_err(|e| e.to_string())?;
		let n = limit.to_string();
		let out = git
			.run(&[
				"log",
				// An unborn HEAD has an empty history. Other git errors still propagate.
				"--ignore-missing",
				"HEAD",
				"-n",
				&n,
				"--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e",
			])
			.map_err(|e| e.to_string())?;
		Ok(browser::parse_log(&String::from_utf8_lossy(&out)))
	})
	.await
}

#[tauri::command]
pub async fn browse_history(
	repo: PathBuf,
	reference: Option<String>,
	query: String,
	skip: usize,
) -> CmdResult<RepositoryHistory> {
	blocking(move || {
		let git = Git::open(&repo).map_err(|e| e.to_string())?;
		browser::history(&git, reference.as_deref(), &query, skip, 300)
			.map_err(|e| e.to_string())
	})
	.await
}

#[tauri::command]
pub async fn browse_directory(
	repo: PathBuf,
	path: String,
) -> CmdResult<Vec<DirectoryEntry>> {
	blocking(move || {
		browser::directory(&repo, &path).map_err(|e| e.to_string())
	})
	.await
}

#[tauri::command]
pub async fn preview_source(
	repo: PathBuf,
	path: String,
	source: Option<GitSourceDto>,
) -> CmdResult<SourcePreview> {
	blocking(move || match source {
		Some(source) => {
			let git = Git::open(&repo).map_err(|e| e.to_string())?;
			browser::git_preview(&git, &source.into(), &path)
				.map_err(|e| e.to_string())
		}
		None => browser::file_preview(&repo, &path).map_err(|e| e.to_string()),
	})
	.await
}

struct FsProbe;

impl DirProbe for FsProbe {
	fn is_dir(&self, abs_path: &str) -> bool {
		Path::new(abs_path).is_dir()
	}

	fn child_dirs(&self, root_abs_path: &str) -> Vec<String> {
		fs::read_dir(root_abs_path)
			.map(|it| {
				it.flatten()
					.filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
					.map(|e| e.file_name().to_string_lossy().into_owned())
					.collect()
			})
			.unwrap_or_default()
	}
}

/// TS `isRelativeEntryPath`: not POSIX-absolute, drive-rooted or UNC.
fn is_relative_entry_path(p: &str) -> bool {
	let b = p.as_bytes();
	let drive = b.len() >= 3
		&& b[0].is_ascii_alphabetic()
		&& b[1] == b':'
		&& (b[2] == b'/' || b[2] == b'\\');
	!p.is_empty() && !p.starts_with('/') && !p.starts_with('\\') && !drive
}

fn plan_clipboard(
	roots: &[PathBuf],
	restore_base: Option<&RestoreBase>,
	header_format: &str,
) -> CmdResult<(ClipboardPlan, Pending)> {
	let Some(primary) = roots.first() else {
		return Err("No workspace folder found.".into());
	};
	let text = clip::read_text().map_err(|e| e.to_string())?;
	if text.trim().is_empty() {
		return Err("Clipboard is empty or does not contain text.".into());
	}

	if clip::detect_mode(&text) == Mode::Commits {
		let payload =
			commits::parse_commit_payload(&text).map_err(|e| e.to_string())?;
		let git = Git::open(primary).map_err(|e| e.to_string())?;
		let plan = commits::plan_commit_replay(&git, &payload);
		let pending = Pending::Commits {
			root: git.root().to_path_buf(),
			payload,
			plan: plan.clone(),
		};
		return Ok((ClipboardPlan::Commits { plan }, pending));
	}

	let mut entries = parse_clipboard(&text, header_format);
	if entries.is_empty() {
		return Err("No Snipcode file headers found in clipboard.".into());
	}
	// Single root only, as TS: multi-root paths carry root labels. Once a
	// base was accepted there is nothing left to offer.
	let suggestion = if roots.len() == 1 && restore_base.is_none() {
		let paths: Vec<String> =
			entries.iter().map(|e| e.path.clone()).collect();
		suggest_restore_base(
			&primary.to_string_lossy(),
			&paths,
			&FsProbe,
			extract_source_root(&text).as_deref(),
		)
	} else {
		None
	};
	if let (Some(base), 1) = (restore_base, roots.len()) {
		for e in &mut entries {
			if is_relative_entry_path(&e.path) {
				e.path = apply_restore_base(base, &e.path);
			}
		}
	}
	let plan = plan_restore(roots, &entries);
	let pending = Pending::Files(plan.clone());
	Ok((ClipboardPlan::Files { plan, suggestion }, pending))
}

/// Reads the clipboard, detects its mode and plans the paste. The plan is
/// kept for `apply_restore` / `replay_commits` / `diff`.
#[tauri::command]
pub async fn read_clipboard_plan(
	app: AppHandle,
	roots: Vec<PathBuf>,
	restore_base: Option<RestoreBase>,
) -> CmdResult<ClipboardPlan> {
	let header_format = load_settings(&app).header_format;
	blocking(move || {
		let state = tauri::Manager::state::<AppState>(&app);
		// A failed preview must not leave an older plan applicable.
		let planned =
			plan_clipboard(&roots, restore_base.as_ref(), &header_format);
		let mut pending = lock(&state.pending);
		*pending = None;
		let (plan, next) = planned?;
		*pending = Some(next);
		Ok(plan)
	})
	.await
}

#[tauri::command]
pub async fn apply_restore(
	app: AppHandle,
	selection: RestoreSelection,
) -> CmdResult<RestoreExecutionResult> {
	blocking(move || {
		let state = tauri::Manager::state::<AppState>(&app);
		let mut pending = lock(&state.pending);
		let Some(Pending::Files(plan)) = pending.take() else {
			return Err("No file restore plan to apply.".into());
		};
		Ok(execute_restore_plan(&plan, &selection))
	})
	.await
}

#[tauri::command]
pub async fn replay_commits(app: AppHandle) -> CmdResult<ReplayResult> {
	blocking(move || {
		let state = tauri::Manager::state::<AppState>(&app);
		let mut pending = lock(&state.pending);
		let Some(Pending::Commits { root, payload, .. }) = pending.take()
		else {
			return Err("No commits to replay.".into());
		};
		let git = Git::open(&root).map_err(|e| e.to_string())?;
		Ok(commits::replay(&git, &payload))
	})
	.await
}

fn current_text(path: Option<&Path>) -> String {
	path.and_then(|p| read_text_file(p).ok().flatten())
		.unwrap_or_default()
}

fn unified(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
	similar::TextDiff::from_lines(old, new)
		.unified_diff()
		.header(old_name, new_name)
		.to_string()
}

/// Unified diff of the current disk file against the pending content. Only
/// paths the plan already resolved and accepted are read.
#[tauri::command]
pub async fn diff(app: AppHandle, target: DiffTarget) -> CmdResult<String> {
	blocking(move || {
		let state = tauri::Manager::state::<AppState>(&app);
		let pending = lock(&state.pending);
		match (&*pending, target) {
			(Some(Pending::Files(plan)), DiffTarget::Restore { index }) => {
				let op = plan
					.create_operations
					.get(index)
					.ok_or("No such restore operation.")?;
				let old = if op.existed {
					current_text(Some(&op.absolute_path))
				} else {
					String::new()
				};
				let name = &op.relative_path;
				Ok(unified(
					&old,
					&op.content,
					&format!("a/{name}"),
					&format!("b/{name}"),
				))
			}
			(
				Some(Pending::Commits { payload, plan, .. }),
				DiffTarget::Commit { commit, file },
			) => {
				let (Some(f), Some(fp)) = (
					payload.commits.get(commit).and_then(|c| c.files.get(file)),
					plan.commits.get(commit).and_then(|c| c.files.get(file)),
				) else {
					return Err("No such commit file.".into());
				};
				// A rename diffs against the old path's content.
				let old_abs = fp
					.old_absolute_path
					.as_deref()
					.or(fp.absolute_path.as_deref());
				let old = current_text(old_abs);
				let new = f.content.clone().unwrap_or_default();
				let old_name = f.old_path.as_deref().unwrap_or(&f.path);
				Ok(unified(
					&old,
					&new,
					&format!("a/{old_name}"),
					&format!("b/{}", f.path),
				))
			}
			_ => Err("Nothing to diff: preview the clipboard first.".into()),
		}
	})
	.await
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_git_log_records() {
		let out =
			"aaa\0p1 p2\0Ann\0a@x\x002026-01-01T00:00:00+08:00\0subj one\x1e\n\
			bbb\0\0Bob\0b@x\x002026-01-02T00:00:00Z\0\x1e\n";
		let got = browser::parse_log(out);
		assert_eq!(got.len(), 2);
		assert_eq!(got[0].parents, ["p1", "p2"]);
		assert_eq!(got[0].subject, "subj one");
		assert!(got[1].parents.is_empty());
		assert_eq!(got[1].author_name, "Bob");
	}

	#[test]
	fn relative_entry_paths() {
		assert!(is_relative_entry_path("src/a.ts"));
		for p in ["", "/abs", "C:/x", "c:\\x", "\\\\srv\\x"] {
			assert!(!is_relative_entry_path(p), "{p}");
		}
	}

	/// `bun run generate:dto` runs this; ignored by plain `cargo test`.
	#[test]
	#[ignore]
	fn export_dto() {
		let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../src/generated");
		let cfg = ts_rs::Config::new()
			.with_out_dir(dir)
			.with_large_int("number");
		CopyRequest::export_all(&cfg).unwrap();
		GitBrowse::export_all(&cfg).unwrap();
		RepositoryHistory::export_all(&cfg).unwrap();
		DirectoryEntry::export_all(&cfg).unwrap();
		SourcePreview::export_all(&cfg).unwrap();
		CopyOutcome::export_all(&cfg).unwrap();
		CopyDone::export_all(&cfg).unwrap();
		CommitSelection::export_all(&cfg).unwrap();
		CommitSummary::export_all(&cfg).unwrap();
		ClipboardPlan::export_all(&cfg).unwrap();
		DiffTarget::export_all(&cfg).unwrap();
		RestoreBase::export_all(&cfg).unwrap();
		RestoreSelection::export_all(&cfg).unwrap();
		RestoreExecutionResult::export_all(&cfg).unwrap();
		ReplayResult::export_all(&cfg).unwrap();
	}
}
