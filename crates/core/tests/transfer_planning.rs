//! Integration tests for P4 safe transfer planning core.
//!
//! Validates:
//! 1. crossrepo samefilename distinctprefix roundtriptargetmapping
//! 2. reorderdestinationroots stilltargetsrightroot
//! 3. ambiguoussamebasenamesblock
//! 4. stagedvsworking conflict
//! 5. nestedprefixonce
//! 6. deleteandwrite mapconsistently
//! 7. HEAD/index/working/destinationchange afterpreview returnsstale andwritesnothing
//! 8. unchangedexactcore restore roundtrip passes
//! 9. commit range selections reject crossrepo and noncontiguous first-parent replay
//! 10. payload limit complete-error not partial success
//! 11. unmapped and blocked entries skipped
//! 12. missing index to new index detects transition
//! 13. same OID branch switch detected by symbolic ref
//! 14. older commit exact content and deletion parent semantics
//! 15. outside path and traversal rejected before IO
//! 16. duplicate mapped destination target rejected before write
//! 17. serialized overhead limit and bounded reads
//! 18. settings filter excludes matching files
//! 19. aggregate-many-files with tiny budget (prove early read stop before retaining)
//! 20. oversized staged/deleted blob before read
//! 21. working file read cap (grows past limit)
//! 22. revision freeze (two files from older revision extracted from frozen OID not HEAD)
//! 23. missing root rejected at boundaries
//! 24. no silent freshness read error
//! 25. alias target collision (inside-root symlink alias two headers collision Linux test)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use snip_core::copy;
use snip_core::format::{self, ChangeType};
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::{self, Git, GitSource};
use snip_core::restore::{
	self, RestoreBase, RestoreBaseSuggestion, RestoreSelection, SkipReason,
};
use snip_core::settings::{FilterAction, FilterRule, FilterType, Settings};
use snip_core::transfer::{
	changed_items, detect_clipboard_prefixes, expand_folder_items,
	plan_commit_export, plan_commit_export_exact, plan_commit_export_with,
	plan_export, plan_import, plan_import_with, selection_from_paths,
	validate_commit_selection, CanonicalRootId, CommitReplayPreview,
	DestinationFreshnessSnapshot, ExportItem, ExportSelection, ImportMapping,
	SourceFreshnessSnapshot, SourceKind, TransferError, CLIPBOARD_PAYLOAD_MAX,
};

struct TestRepo {
	_dir: tempfile::TempDir,
	repo_path: PathBuf,
	cfg: PathBuf,
}

impl TestRepo {
	fn new(name: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let cfg = dir.path().join("empty.gitconfig");
		fs::write(&cfg, "").unwrap();
		let raw_path = dir.path().join(name);
		fs::create_dir_all(&raw_path).unwrap();
		let repo_path = dunce::canonicalize(&raw_path).unwrap_or(raw_path);
		let repo = Self {
			_dir: dir,
			repo_path,
			cfg,
		};
		repo.git(&["init", "-q", "-b", "main"]);
		repo.git(&["config", "user.name", "Test User"]);
		repo.git(&["config", "user.email", "test@example.com"]);
		repo.git(&["config", "core.autocrlf", "false"]);
		repo.git(&["config", "commit.gpgsign", "false"]);
		repo
	}

	fn path(&self) -> &Path {
		&self.repo_path
	}

	fn canonical_id(&self) -> CanonicalRootId {
		CanonicalRootId::new(&self.repo_path).unwrap()
	}

	fn open(&self) -> Git {
		Git::open(&self.repo_path).unwrap()
	}

	fn git(&self, args: &[&str]) -> String {
		let mut cmd = Command::new("git");
		cmd.args(args)
			.current_dir(&self.repo_path)
			.env("GIT_CONFIG_GLOBAL", &self.cfg)
			.env("LC_ALL", "C");
		let out = cmd.output().unwrap();
		assert!(
			out.status.success(),
			"git {:?} in {:?} failed: {}",
			args,
			self.repo_path,
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8_lossy(&out.stdout).trim().to_string()
	}

	fn write(&self, rel: &str, content: &str) {
		let target = self.repo_path.join(rel);
		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent).unwrap();
		}
		fs::write(&target, content).unwrap();
	}

	fn read(&self, rel: &str) -> String {
		fs::read_to_string(self.repo_path.join(rel)).unwrap()
	}

	fn exists(&self, rel: &str) -> bool {
		self.repo_path.join(rel).exists()
	}

	fn commit(&self, msg: &str) -> String {
		self.git(&["add", "-A"]);
		self.git(&["commit", "-m", msg]);
		self.git(&["rev-parse", "HEAD"])
	}
}

// ---------------------------------------------------------------------------
// 1. Cross-repo same filename with distinct prefix round-trip target mapping
// ---------------------------------------------------------------------------

#[test]
fn test_crossrepo_samefilename_distinctprefix_roundtriptargetmapping() {
	let src1 = TestRepo::new("repo-alpha");
	let src2 = TestRepo::new("repo-beta");

	src1.write("src/lib.rs", "pub fn alpha_fn() {}");
	src2.write("src/lib.rs", "pub fn beta_fn() {}");
	src1.commit("init alpha");
	src2.commit("init beta");

	let selection = ExportSelection::new(
		vec![src1.path().to_path_buf(), src2.path().to_path_buf()],
		Some(src1.path().to_path_buf()),
		vec![
			ExportItem {
				root: src1.canonical_id(),
				relative_path: "src/lib.rs".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
			ExportItem {
				root: src2.canonical_id(),
				relative_path: "src/lib.rs".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
		],
	)
	.unwrap();

	let export_plan =
		plan_export(&selection, &Settings::default(), None).unwrap();

	// Primary root is unprefixed, secondary root is prefixed with its basename "repo-beta"
	assert!(export_plan
		.payload
		.contains("// file: [MODIFIED] src/lib.rs"));
	assert!(export_plan
		.payload
		.contains("// file: [MODIFIED] repo-beta/src/lib.rs"));

	let prefixes = detect_clipboard_prefixes(
		&export_plan.payload,
		&Settings::default().header_format,
	);
	assert!(prefixes.contains(&"repo-beta".to_string()));

	// Now import into two completely different destination repos
	let dst1 = TestRepo::new("dest-one");
	let dst2 = TestRepo::new("dest-two");

	let mut mapping = ImportMapping::with_primary(dst1.canonical_id());
	mapping.map_prefix("repo-beta", dst2.canonical_id());

	let destination_roots =
		vec![dst1.path().to_path_buf(), dst2.path().to_path_buf()];
	let import_plan = plan_import(
		&export_plan.payload,
		&Settings::default().header_format,
		&destination_roots,
		&mapping,
	)
	.unwrap();

	let res = import_plan.apply(&RestoreSelection::default()).unwrap();
	assert_eq!(res.created_count, 2);
	assert!(res.errors.is_empty());

	// Verify exact contents landed in the correct mapped repos
	assert_eq!(dst1.read("src/lib.rs"), "pub fn alpha_fn() {}");
	assert_eq!(dst2.read("src/lib.rs"), "pub fn beta_fn() {}");
}

// ---------------------------------------------------------------------------
// 2. Reordering destination roots array still targets the right root
// ---------------------------------------------------------------------------

#[test]
fn test_reorderdestinationroots_stilltargetsrightroot() {
	let dst_primary = TestRepo::new("dest-primary");
	let dst_secondary = TestRepo::new("dest-secondary");

	let clipboard_text = "\
// file: src/main.rs
fn main() { println!(\"primary\"); }
// file: sec/config.json
{\"repo\": \"secondary\"}
";

	let mut mapping = ImportMapping::with_primary(dst_primary.canonical_id());
	mapping.map_prefix("sec", dst_secondary.canonical_id());

	// Pass destination roots in REVERSED order: secondary is first, primary is second
	let reordered_roots = vec![
		dst_secondary.path().to_path_buf(),
		dst_primary.path().to_path_buf(),
	];

	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&reordered_roots,
		&mapping,
	)
	.unwrap();

	let res = import_plan.apply(&RestoreSelection::default()).unwrap();
	assert_eq!(res.created_count, 2);
	assert!(res.errors.is_empty());

	// The primary unprefixed file MUST land in dst_primary, NOT dst_secondary!
	assert!(dst_primary.exists("src/main.rs"));
	assert_eq!(
		dst_primary.read("src/main.rs"),
		"fn main() { println!(\"primary\"); }"
	);
	assert!(!dst_secondary.exists("src/main.rs"));

	// The prefixed file MUST land in dst_secondary
	assert!(dst_secondary.exists("config.json"));
	assert_eq!(
		dst_secondary.read("config.json"),
		"{\"repo\": \"secondary\"}"
	);
	assert!(!dst_primary.exists("config.json"));
}

// ---------------------------------------------------------------------------
// 3. Ambiguous same basenames block combined export
// ---------------------------------------------------------------------------

#[test]
fn test_ambiguoussamebasenamesblock() {
	// Two distinct repos having the same basename "service"
	let repo1 = TestRepo::new("service");
	let repo2 = TestRepo::new("service");

	assert_ne!(repo1.path(), repo2.path());

	let selection = ExportSelection::new(
		vec![repo1.path().to_path_buf(), repo2.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo1.canonical_id(),
				relative_path: "file1.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo2.canonical_id(),
				relative_path: "file2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	);

	// ExportSelection::new validates selection immediately
	let err = selection.unwrap_err();
	match err {
		TransferError::AmbiguousRootBasenames { basename, roots } => {
			assert_eq!(basename, "service");
			assert_eq!(roots.len(), 2);
		}
		other => panic!("expected AmbiguousRootBasenames, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 4. Staged vs Working conflict
// ---------------------------------------------------------------------------

#[test]
fn test_stagedvsworking_conflict() {
	let repo = TestRepo::new("myrepo");
	repo.write("file.txt", "v1");
	repo.commit("c1");
	repo.write("file.txt", "v2-staged");
	repo.git(&["add", "file.txt"]);
	repo.write("file.txt", "v3-working");

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file.txt".to_string(),
				source: SourceKind::Staged,
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file.txt".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
		],
	);

	let err = selection.unwrap_err();
	match err {
		TransferError::StagedWorkingConflict { path, .. } => {
			assert_eq!(path, "file.txt");
		}
		other => panic!("expected StagedWorkingConflict, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 5. Nested prefix consumed exactly once
// ---------------------------------------------------------------------------

#[test]
fn test_nestedprefixonce() {
	let dst = TestRepo::new("dest-pkg");

	let clipboard_text = "\
// file: pkg/nested/pkg/file.txt
pkg content
";

	let mut mapping = ImportMapping::new();
	mapping.map_prefix("pkg", dst.canonical_id());

	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	let res = import_plan.apply(&RestoreSelection::default()).unwrap();
	assert_eq!(res.created_count, 1);

	// "pkg/" was stripped exactly once, keeping "nested/pkg/file.txt"
	assert!(dst.exists("nested/pkg/file.txt"));
	assert_eq!(dst.read("nested/pkg/file.txt"), "pkg content");
	assert!(!dst.exists("nested/file.txt"));
}

// ---------------------------------------------------------------------------
// 6. Delete and write map consistently
// ---------------------------------------------------------------------------

#[test]
fn test_deleteandwrite_mapconsistently() {
	let dst = TestRepo::new("service-target");
	dst.write("old.txt", "to be deleted");
	dst.commit("initial");

	let clipboard_text = "\
// file: [DELETED] srv/old.txt
// file: [NEW] srv/new.txt
newly created file
";

	let mut mapping = ImportMapping::new();
	mapping.map_prefix("srv", dst.canonical_id());

	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	assert_eq!(import_plan.delete_operations().len(), 1);
	assert_eq!(import_plan.create_operations().len(), 1);

	assert_eq!(import_plan.delete_operations()[0].relative_path, "old.txt");
	assert_eq!(import_plan.create_operations()[0].relative_path, "new.txt");

	let res = import_plan.apply(&RestoreSelection::default()).unwrap();
	assert_eq!(res.deleted_count, 1);
	assert_eq!(res.created_count, 1);
	assert!(res.errors.is_empty());

	assert!(!dst.exists("old.txt"));
	assert_eq!(dst.read("new.txt"), "newly created file");
}

// ---------------------------------------------------------------------------
// 7. HEAD/index/working/destination change after preview returns stale and writes nothing
// ---------------------------------------------------------------------------

#[test]
fn test_head_change_afterpreview_returnsstale_andwritesnothing() {
	let dst = TestRepo::new("dest-repo");
	dst.write("init.txt", "v1");
	dst.commit("commit 1");

	let clipboard_text = "\
// file: new_from_preview.txt
previewed content
";
	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// Advance HEAD externally
	dst.write("external.txt", "agent edit");
	dst.commit("commit 2");

	// Applying the previewed plan must fail stale and write nothing!
	let err = import_plan.apply(&RestoreSelection::default()).unwrap_err();
	match err {
		TransferError::StaleDestination { reason, .. } => {
			assert!(reason.contains("HEAD commit changed"));
		}
		other => panic!("expected StaleDestination, got {other:?}"),
	}
	assert!(!dst.exists("new_from_preview.txt"));
}

#[test]
fn test_index_change_afterpreview_returnsstale_andwritesnothing() {
	let dst = TestRepo::new("dest-repo");
	dst.write("init.txt", "v1");
	dst.commit("commit 1");

	let clipboard_text = "\
// file: staged_test.txt
content
";
	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// External agent stages a change
	dst.write("staged.txt", "staged by agent");
	dst.git(&["add", "staged.txt"]);

	let err = import_plan.apply(&RestoreSelection::default()).unwrap_err();
	match err {
		TransferError::StaleDestination { reason, .. } => {
			assert!(reason.contains("index changed"));
		}
		other => panic!("expected StaleDestination, got {other:?}"),
	}
	assert!(!dst.exists("staged_test.txt"));
}

#[test]
fn test_working_file_content_hash_change_detects_samesize_edits() {
	let src = TestRepo::new("src-repo");
	src.write("file.txt", "AAAA"); // 4 bytes

	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: src.canonical_id(),
			relative_path: "file.txt".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let snapshot = SourceFreshnessSnapshot::capture(&selection).unwrap();
	assert!(snapshot.revalidate().is_ok());

	// Modify the file with the EXACT SAME byte length
	src.write("file.txt", "BBBB"); // 4 bytes

	let err = snapshot.revalidate().unwrap_err();
	match err {
		TransferError::StaleSource { reason, .. } => {
			assert!(reason.contains("modified or deleted"));
		}
		other => panic!("expected StaleSource, got {other:?}"),
	}
}

#[test]
fn test_destination_file_modified_afterpreview_returnsstale_andwritesnothing() {
	let dst = TestRepo::new("dest-repo");
	dst.write("target.txt", "original content");

	let clipboard_text = "\
// file: target.txt
updated by paste
";
	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// Target file is modified externally before apply
	dst.write("target.txt", "modified externally");

	let err = import_plan.apply(&RestoreSelection::default()).unwrap_err();
	match err {
		TransferError::StaleDestination { reason, .. } => {
			assert!(reason.contains("modified externally"));
		}
		other => panic!("expected StaleDestination, got {other:?}"),
	}

	// Verify file was NOT overwritten with "updated by paste"
	assert_eq!(dst.read("target.txt"), "modified externally");
}

// ---------------------------------------------------------------------------
// 8. Unchanged exact core restore roundtrip passes
// ---------------------------------------------------------------------------

#[test]
fn test_unchangedexactcore_restore_roundtrip_passes() {
	let dst = TestRepo::new("target-repo");
	dst.write("existing.txt", "keep me");

	let clipboard_text = "\
// file: [NEW] added.txt
hello world
// file: [MODIFIED] existing.txt
replaced content
";

	// Compare with legacy core restore plan
	let legacy_entries =
		format::parse_clipboard(clipboard_text, "// file: $FILE_PATH");
	let legacy_plan = restore::plan_restore(&[dst.path()], &legacy_entries);

	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let transfer_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	assert_eq!(
		transfer_plan.create_operations().len(),
		legacy_plan.create_operations.len()
	);
	for (op_new, op_leg) in transfer_plan
		.create_operations()
		.iter()
		.zip(&legacy_plan.create_operations)
	{
		assert_eq!(op_new.relative_path, op_leg.relative_path);
		assert_eq!(op_new.content, op_leg.content);
		assert_eq!(op_new.existed, op_leg.existed);
	}

	let res = transfer_plan
		.apply(&RestoreSelection {
			overwrite_existing: true,
			..RestoreSelection::default()
		})
		.unwrap();

	assert_eq!(res.created_count, 1);
	assert_eq!(res.overwritten_count, 1);
	assert_eq!(dst.read("added.txt"), "hello world");
	assert_eq!(dst.read("existing.txt"), "replaced content");
}

// ---------------------------------------------------------------------------
// 9. Commit range selection: cross-repo rejected, discontinuous rejected
// ---------------------------------------------------------------------------

#[test]
fn test_commit_selection_crossrepo_and_discontinuous_rejected() {
	let r1 = TestRepo::new("repo-1");
	let r2 = TestRepo::new("repo-2");
	let g1 = r1.open();
	let g2 = r2.open();

	// Cross-repo commit selection rejected
	let err = validate_commit_selection(&[&g1, &g2]).unwrap_err();
	assert!(matches!(err, TransferError::CrossRepoCommitsNotSupported));

	// Non-contiguous replay rejection along first parents
	r1.write("a.txt", "1");
	let c1 = r1.commit("commit 1");

	r1.git(&["checkout", "-b", "side"]);
	r1.write("side.txt", "side");
	let side_tip = r1.commit("side commit");

	r1.git(&["checkout", "main"]);
	r1.write("b.txt", "2");
	r1.commit("commit 2");
	r1.git(&["merge", "side", "-m", "merge side"]);
	let main_tip = r1.git(&["rev-parse", "HEAD"]);

	// Attempting to select range from side_tip to main_tip is non-contiguous
	// on main's first-parent chain!
	let err = plan_commit_export(&g1, Some((&side_tip, &main_tip)), None)
		.unwrap_err();
	match err {
		TransferError::DiscontinuousCommits { at, base, tip, .. } => {
			assert_eq!(base, side_tip);
			assert_eq!(tip, main_tip);
			assert!(!at.is_empty());
		}
		other => panic!("expected DiscontinuousCommits, got: {other:?}"),
	}

	// Contiguous range c1..main_tip succeeds
	let payload =
		plan_commit_export(&g1, Some((&c1, &main_tip)), None).unwrap();
	assert!(!payload.commits.is_empty());
}

// ---------------------------------------------------------------------------
// 10. Payload limit: complete error, not partial success
// ---------------------------------------------------------------------------

#[test]
fn test_file_count_limit_retains_and_flags() {
	let r = TestRepo::new("repo");
	r.write("f1.txt", "a");
	r.write("f2.txt", "b");

	let settings = Settings {
		set_max_file_count: true,
		file_count_limit: 1.0,
		..Settings::default()
	};

	let selection = ExportSelection::new(
		vec![r.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: r.canonical_id(),
				relative_path: "f1.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: r.canonical_id(),
				relative_path: "f2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].path, "f1.txt");
	assert!(plan.file_limit_reached);
	assert_eq!(plan.copied_file_count, 1);
}

// ---------------------------------------------------------------------------
// 11. Unmapped entries and blocked prefixes
// ---------------------------------------------------------------------------

#[test]
fn test_unmapped_and_blocked_entries_skipped() {
	let dst = TestRepo::new("target");

	let clipboard_text = "\
// file: unknown_prefix/file1.txt
content 1
// file: blocked_prefix/file2.txt
content 2
";

	let mut mapping = ImportMapping::new();
	// Note: no primary_destination set, blocked_prefix set
	mapping.block_prefix("blocked_prefix");

	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// Both entries should be skipped as UnresolvedPath
	assert_eq!(import_plan.create_operations().len(), 0);
	assert_eq!(import_plan.skipped_operations().len(), 2);
	for op in import_plan.skipped_operations() {
		assert_eq!(op.reason, SkipReason::UnresolvedPath);
	}
}

// ---------------------------------------------------------------------------
// 12. Missing index to new index transition detected
// ---------------------------------------------------------------------------

#[test]
fn test_missingindex_to_newindex_transition_detected() {
	let dst = TestRepo::new("repo-index-test");
	dst.write("initial.txt", "init");
	dst.commit("c1");

	let clipboard_text = "\
// file: new.txt
content
";
	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// Simulate index being deleted externally
	let index_file = dst.path().join(".git/index");
	if index_file.exists() {
		fs::remove_file(&index_file).unwrap();
	}

	// Capture freshness when index is missing
	let snapshot = DestinationFreshnessSnapshot::capture(
		&[dst.path().to_path_buf()],
		import_plan.restore_plan(),
	)
	.unwrap();

	// External agent creates index again by staging a file
	dst.git(&["add", "initial.txt"]);
	assert!(index_file.exists());

	// Revalidating snapshot must catch None -> Some transition!
	let err = snapshot.revalidate().unwrap_err();
	assert!(matches!(err, TransferError::StaleDestination { .. }));
}

// ---------------------------------------------------------------------------
// 13. Same OID branch switch detected by symbolic ref
// ---------------------------------------------------------------------------

#[test]
fn test_sameoid_branchswitch_detected() {
	let dst = TestRepo::new("repo-branch-test");
	dst.write("initial.txt", "init");
	dst.commit("c1");

	// Create side branch at identical OID
	dst.git(&["branch", "side"]);

	let clipboard_text = "\
// file: new.txt
content
";
	let mapping = ImportMapping::with_primary(dst.canonical_id());
	let import_plan = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap();

	// Switch branch to `side` (same commit OID!)
	dst.git(&["checkout", "side"]);

	let err = import_plan.apply(&RestoreSelection::default()).unwrap_err();
	match err {
		TransferError::StaleDestination { reason, .. } => {
			assert!(reason.contains("branch/ref changed"));
		}
		other => {
			panic!("expected StaleDestination with ref change, got {other:?}")
		}
	}
	assert!(!dst.exists("new.txt"));
}

// ---------------------------------------------------------------------------
// 14. Older commit exact content and deletion parent semantics
// ---------------------------------------------------------------------------

#[test]
fn test_oldercommit_exactcontent_and_deletion_semantics() {
	let src = TestRepo::new("history-repo");
	src.write("file.txt", "v1 content");
	src.write("deleted_later.txt", "historical deleted body");
	let _c1 = src.commit("commit 1");

	src.write("file.txt", "v2 commit2 content");
	src.git(&["rm", "deleted_later.txt"]);
	let c2 = src.commit("commit 2");

	src.write("file.txt", "v3 HEAD content");
	let _c3 = src.commit("commit 3");

	// Selection selects the OLD commit c2, NOT HEAD c3!
	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: src.canonical_id(),
				relative_path: "file.txt".to_string(),
				source: SourceKind::Commit { rev: c2.clone() },
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
			ExportItem {
				root: src.canonical_id(),
				relative_path: "deleted_later.txt".to_string(),
				source: SourceKind::Commit { rev: c2 },
				change_type: Some(ChangeType::Deleted),
				gitlink: false,
			},
		],
	)
	.unwrap();

	let export_plan =
		plan_export(&selection, &Settings::default(), None).unwrap();

	// Content must be v2 content from old commit c2, NOT "v3 HEAD content"
	assert!(export_plan.payload.contains("v2 commit2 content"));
	assert!(!export_plan.payload.contains("v3 HEAD content"));
	assert!(export_plan.payload.contains("historical deleted body"));
}

/// A deleted file reads like gitsrc: every parent in order, and the
/// deleted marker when no parent has decodable text (binary included).
#[test]
fn test_commit_deletions_read_every_parent_and_mark_binary() {
	let src = TestRepo::new("merge-deletions");
	src.write("base.txt", "base\n");
	src.commit("base");
	src.git(&["checkout", "-q", "-b", "side"]);
	src.write("side_only.txt", "only on side\n");
	fs::write(src.path().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
	src.commit("side");
	src.git(&["checkout", "-q", "-"]);
	src.git(&["merge", "-q", "--no-ff", "--no-commit", "side"]);
	src.git(&["rm", "-q", "-f", "side_only.txt", "blob.bin"]);
	let merge = src.commit("merge drops side files");

	let item = |path: &str, source: SourceKind| ExportItem {
		root: src.canonical_id(),
		relative_path: path.to_string(),
		source,
		change_type: Some(ChangeType::Deleted),
		gitlink: false,
	};
	let commit = || SourceKind::Commit { rev: merge.clone() };
	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![item("side_only.txt", commit()), item("blob.bin", commit())],
	)
	.unwrap();
	let plan = plan_export(&selection, &Settings::default(), None).unwrap();
	assert!(plan.payload.contains("only on side"), "{}", plan.payload);
	assert_eq!(plan.files.len(), 2, "{}", plan.payload);
	assert_eq!(plan.files[1].path, "blob.bin");
	assert_eq!(
		plan.files[1].content.as_deref(),
		Some(gitsrc::DELETED_FILE_MARKER)
	);

	// A staged binary deletion is the marker too.
	src.git(&["rm", "-q", "base.txt"]);
	fs::write(src.path().join("b2.bin"), [0u8, 200]).unwrap();
	src.git(&["add", "b2.bin"]);
	src.commit("add b2");
	src.git(&["rm", "-q", "-f", "b2.bin"]);
	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![item("b2.bin", SourceKind::Staged)],
	)
	.unwrap();
	let plan = plan_export(&selection, &Settings::default(), None).unwrap();
	assert_eq!(plan.files.len(), 1, "{}", plan.payload);
	assert_eq!(
		plan.files[0].content.as_deref(),
		Some(gitsrc::DELETED_FILE_MARKER)
	);
}

#[test]
fn test_deleted_binary_over_cap_is_the_marker_like_gitsrc() {
	let repo = TestRepo::new("deleted-binary-over-cap");
	let mut big_bytes = vec![b'x'; 200_000];
	big_bytes[100] = 0;
	fs::write(repo.path().join("big.bin"), &big_bytes).unwrap();
	repo.commit("add big binary");
	repo.git(&["rm", "-q", "-f", "big.bin"]);
	let del_rev = repo.commit("delete big binary");

	let item1 = ExportItem {
		root: repo.canonical_id(),
		relative_path: "big.bin".to_string(),
		source: SourceKind::Commit {
			rev: del_rev.clone(),
		},
		change_type: Some(ChangeType::Deleted),
		gitlink: false,
	};
	let sel1 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![item1],
	)
	.unwrap();
	let plan1 = plan_export(&sel1, &Settings::default(), Some(4096)).unwrap();
	assert_eq!(
		plan1.files[0].content.as_deref(),
		Some(gitsrc::DELETED_FILE_MARKER)
	);

	let git_payload = gitsrc::collect_payload(
		&repo.open(),
		&GitSource::Commit(del_rev),
		&[repo.path().to_path_buf()],
		&Settings::default(),
	)
	.unwrap();
	let gitsrc_big = git_payload
		.files
		.iter()
		.find(|f| f.path == "big.bin")
		.expect("big.bin in gitsrc files");
	assert_eq!(plan1.files[0].content, gitsrc_big.content);

	let mut b2_bytes = vec![b'y'; 2048];
	b2_bytes[50] = 0;
	fs::write(repo.path().join("b2.bin"), &b2_bytes).unwrap();
	repo.git(&["add", "b2.bin"]);
	repo.commit("add b2");
	repo.git(&["rm", "-q", "-f", "b2.bin"]);

	let item2 = ExportItem {
		root: repo.canonical_id(),
		relative_path: "b2.bin".to_string(),
		source: SourceKind::Staged,
		change_type: Some(ChangeType::Deleted),
		gitlink: false,
	};
	let sel2 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![item2],
	)
	.unwrap();
	let tiny_settings = Settings {
		max_file_size_kb: 1.0,
		..Settings::default()
	};
	let plan2 = plan_export(&sel2, &tiny_settings, None).unwrap();
	assert_eq!(
		plan2.files[0].content.as_deref(),
		Some(gitsrc::DELETED_FILE_MARKER)
	);
	assert_eq!(plan2.skipped_file_size_count, 0);
}

// ---------------------------------------------------------------------------
// 15. Outside path and traversal rejected before IO
// ---------------------------------------------------------------------------

#[test]
fn test_outsidepath_and_traversal_rejected_before_io() {
	let repo = TestRepo::new("safe-repo");
	repo.write("file.txt", "hello");

	// Case A: Path traversal with ..
	let bad_selection1 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "../outside.txt".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	);
	assert!(matches!(
		bad_selection1.unwrap_err(),
		TransferError::UnsafePath(_)
	));

	// Case B: Absolute path
	let bad_selection2 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "/etc/passwd".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	);
	assert!(matches!(
		bad_selection2.unwrap_err(),
		TransferError::UnsafePath(_)
	));
}

// ---------------------------------------------------------------------------
// 16. Duplicate mapped destination target rejected before write
// ---------------------------------------------------------------------------

#[test]
fn test_duplicate_mapped_destination_target_rejected() {
	let dst = TestRepo::new("collision-target");

	let clipboard_text = "\
// file: item1.txt
content 1
// file: item2.txt
content 2
";

	// Both item1 and item2 explicitly map to the SAME destination path "target.txt"
	let mut mapping = ImportMapping::new();
	mapping.map_entry(
		"item1.txt",
		dst.canonical_id(),
		Some("target.txt".to_string()),
	);
	mapping.map_entry(
		"item2.txt",
		dst.canonical_id(),
		Some("target.txt".to_string()),
	);

	let err = plan_import(
		clipboard_text,
		"// file: $FILE_PATH",
		&[dst.path().to_path_buf()],
		&mapping,
	)
	.unwrap_err();

	match err {
		TransferError::TargetCollision { path, msg } => {
			assert_eq!(path, dst.canonical_id().path().join("target.txt"));
			assert!(msg.contains("multiple operations target"));
		}
		other => panic!("expected TargetCollision, got {other:?}"),
	}
	assert!(!dst.canonical_id().path().join("target.txt").exists());

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_root = symlink_dir.path().join("symlink_collision_dst");
		std::os::unix::fs::symlink(dst.path(), &symlink_root).unwrap();

		// Even when destination root is spelled through a symlink alias,
		// plan_import canonicalizes it, rejects duplicate target, and reports the canonical target.
		let err_symlink = plan_import(
			clipboard_text,
			"// file: $FILE_PATH",
			&[symlink_root],
			&mapping,
		)
		.unwrap_err();

		match err_symlink {
			TransferError::TargetCollision { path, msg } => {
				assert_eq!(path, dst.canonical_id().path().join("target.txt"));
				assert!(msg.contains("multiple operations target"));
			}
			other => panic!("expected TargetCollision, got {other:?}"),
		}
		assert!(!dst.canonical_id().path().join("target.txt").exists());
	}
}

// ---------------------------------------------------------------------------
// 17. Serialized overhead limit and bounded reads
// ---------------------------------------------------------------------------

#[test]
fn test_serialized_overhead_limit_and_bounded_reads() {
	let repo = TestRepo::new("bound-repo");
	repo.write("small.txt", "tiny");

	// Bounded check: serialized payload includes wrappers/headers
	let settings = Settings {
		header_format: "// file: $FILE_PATH".to_string(),
		pre_text: "A".repeat(200),
		..Settings::default()
	};

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "small.txt".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	// Budget is 50 bytes, but pre_text alone is 200 bytes!
	let err = plan_export(&selection, &settings, Some(50)).unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded {
			limit,
			actual,
			reason,
		} => {
			assert_eq!(limit, 50);
			assert!(actual > 200);
			assert!(reason.contains("serialized wrapper overhead"));
		}
		other => panic!("expected PayloadLimitExceeded, got {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 18. Settings filter excludes matching files
// ---------------------------------------------------------------------------

#[test]
fn test_settings_filter_excludes_matching_files() {
	let repo = TestRepo::new("filter-repo");
	repo.write("src/keep.rs", "pub fn keep() {}");
	repo.write("src/ignore.tmp", "ignore me");

	let settings = Settings {
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Pattern,
			action: FilterAction::Exclude,
			value: "*.tmp".to_string(),
			enabled: true,
		}],
		..Settings::default()
	};

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/keep.rs".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/ignore.tmp".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].path, "src/keep.rs");
	assert!(plan.payload.contains("keep.rs"));
	assert!(!plan.payload.contains("ignore.tmp"));
}

// ---------------------------------------------------------------------------
// 19. Aggregate-many-files with tiny budget (prove early read stop before retaining)
// ---------------------------------------------------------------------------

#[test]
fn test_aggregate_many_files_tiny_budget_stops_early_before_retaining() {
	let repo = TestRepo::new("agg-repo");
	let mut items = Vec::new();
	for i in 0..50 {
		let name = format!("f_{i:02}.txt");
		repo.write(&name, &"A".repeat(1000));
		items.push(ExportItem {
			root: repo.canonical_id(),
			relative_path: name,
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		});
	}

	let selection =
		ExportSelection::new(vec![repo.path().to_path_buf()], None, items)
			.unwrap();

	// Budget is 2500 bytes (room for ~2 files + headers, but definitely not 50 files = 50KB)
	let err =
		plan_export(&selection, &Settings::default(), Some(2500)).unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, 2500);
			assert!(actual > 2500);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 20. Oversized staged and deleted blob before read
// ---------------------------------------------------------------------------

#[test]
fn test_oversized_staged_and_deleted_blob_before_read() {
	let repo = TestRepo::new("blob-repo");

	// Part A: Staged oversized blob
	repo.write("staged_huge.txt", &"S".repeat(50_000));
	repo.git(&["add", "staged_huge.txt"]);

	let staged_selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "staged_huge.txt".to_string(),
			source: SourceKind::Staged,
			change_type: Some(ChangeType::New),
			gitlink: false,
		}],
	)
	.unwrap();

	let err = plan_export(&staged_selection, &Settings::default(), Some(500))
		.unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, 500);
			assert!(actual >= 50_000);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}

	// Commit the file then test deleted oversized blob
	repo.commit("add huge");
	repo.git(&["rm", "staged_huge.txt"]);
	let c2 = repo.commit("delete huge");

	// Part B: Commit-deleted oversized blob (inspects parent commit's blob size)
	let deleted_commit_selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "staged_huge.txt".to_string(),
			source: SourceKind::Commit { rev: c2 },
			change_type: Some(ChangeType::Deleted),
			gitlink: false,
		}],
	)
	.unwrap();

	let err_deleted =
		plan_export(&deleted_commit_selection, &Settings::default(), Some(500))
			.unwrap_err();
	match err_deleted {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, 500);
			assert!(actual >= 50_000);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 21. Working file read cap (grows past limit)
// ---------------------------------------------------------------------------

#[test]
fn test_working_file_read_cap_grows_past_limit() {
	let repo = TestRepo::new("read-cap-repo");
	repo.write("large.txt", &"L".repeat(10_000));

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "large.txt".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	// Case 1: File metadata indicates size > remaining budget -> rejects before read
	let err =
		plan_export(&selection, &Settings::default(), Some(100)).unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, 100);
			assert!(actual >= 10_000);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}

	// Case 2: Regular file growing during read where stream produces > budget
	{
		let file_path = repo.path().join("growing_file.txt");
		fs::write(&file_path, "1234567890").unwrap();
		let stop =
			std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
		let stop_writer = stop.clone();
		let p_clone = file_path.clone();
		let writer = std::thread::spawn(move || {
			use std::io::Write;
			while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
				if let Ok(mut f) =
					fs::OpenOptions::new().append(true).open(&p_clone)
				{
					let _ = f.write_all(&[b'X'; 500]);
				}
				std::thread::yield_now();
			}
		});

		let grow_selection = ExportSelection::new(
			vec![repo.path().to_path_buf()],
			None,
			vec![ExportItem {
				root: repo.canonical_id(),
				relative_path: "growing_file.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			}],
		)
		.unwrap();

		let err = plan_export(&grow_selection, &Settings::default(), Some(50))
			.unwrap_err();
		stop.store(true, std::sync::atomic::Ordering::Relaxed);
		let _ = writer.join();
		match err {
			TransferError::PayloadLimitExceeded { reason, .. } => {
				assert!(
					reason.contains("grew past limit")
						|| reason.contains("exceeds")
				);
			}
			other => {
				panic!("expected PayloadLimitExceeded, got: {other:?}")
			}
		}
	}
}

// ---------------------------------------------------------------------------
// 22. Revision freeze (two files from older revision extracted from frozen OID not HEAD)
// ---------------------------------------------------------------------------

#[test]
fn test_revision_freeze_two_files_older_revision() {
	let repo = TestRepo::new("rev-freeze-repo");
	repo.write("file_a.txt", "v1 file A content");
	repo.write("file_b.txt", "v1 file B content");
	let c_old = repo.commit("initial revision");

	// Now advance HEAD to revision 2 with completely different content
	repo.write("file_a.txt", "v2 HEAD file A content");
	repo.write("file_b.txt", "v2 HEAD file B content");
	let _c_head = repo.commit("second revision");

	// Select both files using symbolic rev "HEAD~1" (or c_old)
	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file_a.txt".to_string(),
				source: SourceKind::Commit {
					rev: "HEAD~1".to_string(),
				},
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file_b.txt".to_string(),
				source: SourceKind::Commit {
					rev: "HEAD~1".to_string(),
				},
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &Settings::default(), None).unwrap();

	// Verify both files have content from the older revision
	assert!(plan.payload.contains("v1 file A content"));
	assert!(plan.payload.contains("v1 file B content"));
	assert!(!plan.payload.contains("v2 HEAD file A content"));
	assert!(!plan.payload.contains("v2 HEAD file B content"));

	// Verify the snapshot resolved and froze the exact 40-character commit OID
	let key = (repo.canonical_id(), "HEAD~1".to_string());
	let frozen = plan.freshness.frozen_commits.get(&key).unwrap();
	assert_eq!(frozen, &c_old);
}

// ---------------------------------------------------------------------------
// 23. Missing root rejected at boundaries
// ---------------------------------------------------------------------------

#[test]
fn test_missing_root_rejected_at_boundaries() {
	let nonexistent =
		PathBuf::from("/nonexistent/workspace/root/that/never/exists");

	// CanonicalRootId::new rejects nonexistent path
	assert!(CanonicalRootId::new(&nonexistent).is_err());

	// ExportSelection::new rejects nonexistent root
	let err = ExportSelection::new(vec![nonexistent.clone()], None, vec![])
		.unwrap_err();
	assert!(matches!(err, TransferError::Io(_)));

	// ExportSelection::new rejects item whose root is not declared in selection.roots
	let r1 = TestRepo::new("r1");
	let r2 = TestRepo::new("r2");
	let undeclared_item = ExportItem {
		root: r2.canonical_id(),
		relative_path: "test.txt".to_string(),
		source: SourceKind::Working,
		change_type: None,
		gitlink: false,
	};
	let err2 = ExportSelection::new(
		vec![r1.path().to_path_buf()],
		None,
		vec![undeclared_item.clone()],
	)
	.unwrap_err();
	match err2 {
		TransferError::UnknownRoot(p) => {
			assert_eq!(p, r2.canonical_id().path())
		}
		other => panic!("expected UnknownRoot, got: {other:?}"),
	}

	// plan_import rejects undeclared destination root
	let mapping = ImportMapping::with_primary(r2.canonical_id());
	let err3 = plan_import(
		"// file: test.txt\ncontent\n",
		"// file: $FILE_PATH",
		&[r1.path().to_path_buf()],
		&mapping,
	)
	.unwrap_err();
	match err3 {
		TransferError::UnknownRoot(p) => {
			assert_eq!(p, r2.canonical_id().path())
		}
		other => panic!("expected UnknownRoot, got: {other:?}"),
	}

	// Deleted root: capture canonical identity BEFORE deletion, then delete from filesystem.
	let deleted_temp = tempfile::tempdir().unwrap();
	let deleted_path = dunce::canonicalize(deleted_temp.path()).unwrap();
	let deleted_id = CanonicalRootId::new(&deleted_path).unwrap();
	let expected_deleted_path = deleted_id.path().to_path_buf();
	// Drop the TempDir so the root no longer exists on disk
	drop(deleted_temp);
	assert!(!expected_deleted_path.exists());

	// Attempting to declare the deleted root in ExportSelection fails because validate() requires existing canonical root
	let err_deleted_selection =
		ExportSelection::new(vec![expected_deleted_path.clone()], None, vec![])
			.unwrap_err();
	assert!(matches!(err_deleted_selection, TransferError::Io(_)));

	// If an undeclared item references this deleted root id, UnknownRoot accurately reports the pre-captured identity
	let undeclared_deleted_item = ExportItem {
		root: deleted_id,
		relative_path: "test.txt".to_string(),
		source: SourceKind::Working,
		change_type: None,
		gitlink: false,
	};
	let err_undeclared_deleted = ExportSelection::new(
		vec![r1.path().to_path_buf()],
		None,
		vec![undeclared_deleted_item],
	)
	.unwrap_err();
	match err_undeclared_deleted {
		TransferError::UnknownRoot(p) => assert_eq!(p, expected_deleted_path),
		other => panic!("expected UnknownRoot, got: {other:?}"),
	}

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let r1_symlink = symlink_dir.path().join("r1_symlink");
		std::os::unix::fs::symlink(r1.path(), &r1_symlink).unwrap();

		let err_symlink = ExportSelection::new(
			vec![r1_symlink.clone()],
			None,
			vec![undeclared_item],
		)
		.unwrap_err();
		match err_symlink {
			TransferError::UnknownRoot(p) => {
				assert_eq!(p, r2.canonical_id().path())
			}
			other => panic!("expected UnknownRoot, got: {other:?}"),
		}

		let err_import_symlink = plan_import(
			"// file: test.txt\ncontent\n",
			"// file: $FILE_PATH",
			&[r1_symlink],
			&mapping,
		)
		.unwrap_err();
		match err_import_symlink {
			TransferError::UnknownRoot(p) => {
				assert_eq!(p, r2.canonical_id().path())
			}
			other => panic!("expected UnknownRoot, got: {other:?}"),
		}
	}
}

// ---------------------------------------------------------------------------
// 24. No silent freshness read error
// ---------------------------------------------------------------------------

#[test]
fn test_no_silent_freshness_read_error() {
	let repo = TestRepo::new("perms-repo");
	repo.write("unreadable.txt", "secret");

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let target = repo.path().join("unreadable.txt");
		let mut perms = fs::metadata(&target).unwrap().permissions();
		perms.set_mode(0o000);
		fs::set_permissions(&target, perms).unwrap();

		let selection = ExportSelection::new(
			vec![repo.path().to_path_buf()],
			None,
			vec![ExportItem {
				root: repo.canonical_id(),
				relative_path: "unreadable.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			}],
		)
		.unwrap();

		// capture() must NOT silently convert PermissionDenied into None!
		let err = SourceFreshnessSnapshot::capture(&selection).unwrap_err();
		assert!(matches!(err, TransferError::Io(_)));

		// Restore permissions so tempdir cleanup succeeds
		let mut restore_perms = fs::metadata(&target).unwrap().permissions();
		restore_perms.set_mode(0o644);
		let _ = fs::set_permissions(&target, restore_perms);
	}
}

// ---------------------------------------------------------------------------
// 25. Alias target collision: inside-root symlink alias two headers collision (Linux)
// ---------------------------------------------------------------------------

#[test]
fn test_alias_target_collision_inside_root_symlink() {
	#[cfg(unix)]
	{
		let dst = TestRepo::new("symlink-target");
		let real_dir = dst.path().join("real_dir");
		fs::create_dir_all(&real_dir).unwrap();
		let link_dir = dst.path().join("link_dir");
		std::os::unix::fs::symlink("real_dir", &link_dir).unwrap();

		let clipboard_text = "\
// file: real_dir/target.txt
first content
// file: link_dir/target.txt
second content
";
		let mapping = ImportMapping::with_primary(dst.canonical_id());
		let err = plan_import(
			clipboard_text,
			"// file: $FILE_PATH",
			&[dst.path().to_path_buf()],
			&mapping,
		)
		.unwrap_err();

		match err {
			TransferError::TargetCollision { path, msg } => {
				assert!(path.to_string_lossy().contains("target.txt"));
				assert!(msg.contains("multiple operations target"));
			}
			other => panic!("expected TargetCollision, got: {other:?}"),
		}

		// Ensure no-write-on-invalid-plan: neither alias was created on disk!
		assert!(!dst.exists("real_dir/target.txt"));
		assert!(!dst.exists("link_dir/target.txt"));
	}
}

// ---------------------------------------------------------------------------
// 26. Parity: plan_export file mode matches copy::collect_copy_files
// ---------------------------------------------------------------------------

#[test]
fn test_parity_plan_export_file_mode_matches_copy_collect() {
	let repo = TestRepo::new("file-parity");
	repo.write("src/a.rs", "fn a() {}\n");
	repo.write("src/b.rs", "fn b() {}\n");
	repo.commit("initial");

	let settings = Settings::default();
	let targets =
		vec![repo.path().join("src/a.rs"), repo.path().join("src/b.rs")];
	let copy_res =
		copy::collect_copy_files(&[repo.path()], &targets, &settings);

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/a.rs".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/b.rs".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.payload, copy_res.payload);
	assert_eq!(plan.copied_file_count, copy_res.copied_file_count);
	assert_eq!(
		plan.skipped_file_size_count,
		copy_res.skipped_file_size_count
	);
	assert_eq!(
		plan.skipped_unreadable_count,
		copy_res.skipped_unreadable_count
	);
	assert_eq!(plan.file_limit_reached, copy_res.file_limit_reached);
}

// ---------------------------------------------------------------------------
// 27. Parity: plan_export staged matches gitsrc::collect_payload_with_selection
// ---------------------------------------------------------------------------

#[test]
fn test_parity_plan_export_staged_matches_gitsrc() {
	let repo = TestRepo::new("staged-parity");
	repo.write("keep.txt", "keep\n");
	repo.write("del.txt", "to be deleted\n");
	repo.commit("base");

	repo.write("new.txt", "new staged file\n");
	repo.git(&["add", "new.txt"]);
	repo.git(&["rm", "del.txt"]);

	let settings = Settings::default();
	let git = repo.open();
	let git_res = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Staged,
		&[repo.path()],
		&settings,
		None,
		None,
	)
	.unwrap();

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "del.txt".to_string(),
				source: SourceKind::Staged,
				change_type: Some(ChangeType::Deleted),
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "new.txt".to_string(),
				source: SourceKind::Staged,
				change_type: Some(ChangeType::New),
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.payload, git_res.payload);
	assert_eq!(plan.copied_file_count, git_res.copied_file_count);
	assert_eq!(
		plan.skipped_file_size_count,
		git_res.skipped_file_size_count
	);
	assert_eq!(
		plan.skipped_unreadable_count,
		git_res.skipped_unreadable_count
	);
	assert_eq!(plan.file_limit_reached, git_res.file_limit_reached);
}

// ---------------------------------------------------------------------------
// 28. Parity: plan_export commit matches gitsrc::collect_payload_with_selection
// ---------------------------------------------------------------------------

#[test]
fn test_parity_plan_export_commit_matches_gitsrc() {
	let repo = TestRepo::new("commit-parity");
	repo.write("del.txt", "delete me\n");
	repo.write("mod.txt", "original\n");
	repo.commit("base");

	repo.git(&["rm", "del.txt"]);
	repo.write("mod.txt", "modified\n");
	let commit_oid = repo.commit("change commit");

	let settings = Settings::default();
	let git = repo.open();
	let git_res = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Commit(commit_oid.clone()),
		&[repo.path()],
		&settings,
		None,
		None,
	)
	.unwrap();

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "del.txt".to_string(),
				source: SourceKind::Commit {
					rev: commit_oid.clone(),
				},
				change_type: Some(ChangeType::Deleted),
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "mod.txt".to_string(),
				source: SourceKind::Commit { rev: commit_oid },
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.payload, git_res.payload);
	assert_eq!(plan.copied_file_count, git_res.copied_file_count);
	assert_eq!(
		plan.skipped_file_size_count,
		git_res.skipped_file_size_count
	);
	assert_eq!(
		plan.skipped_unreadable_count,
		git_res.skipped_unreadable_count
	);
	assert_eq!(plan.file_limit_reached, git_res.file_limit_reached);
}

// ---------------------------------------------------------------------------
// 29. Binary unreadable files skipped without empty headers/bodies
// ---------------------------------------------------------------------------

#[test]
fn test_binary_unreadable_files_skipped_without_empty_header() {
	let repo = TestRepo::new("binary-skip");
	let bin_path = repo.path().join("image.bin");
	fs::write(&bin_path, [0xFF, 0xFE, 0x00, 0x01, 0x80]).unwrap();
	repo.write("valid.txt", "valid text\n");

	let settings = Settings::default();
	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "image.bin".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "valid.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].path, "valid.txt");
	assert_eq!(plan.skipped_unreadable_count, 1);
	assert_eq!(plan.copied_file_count, 1);
	assert!(!plan.payload.contains("image.bin"));
}

// ---------------------------------------------------------------------------
// 30. Budget: repeated headers and escape_content counted before allocation
// ---------------------------------------------------------------------------

#[test]
fn test_budget_repeated_headers_and_escape_content_counted_before_allocation() {
	let repo = TestRepo::new("header-escape-budget");
	let mut body = String::new();
	for i in 0..10 {
		body.push_str(&format!("// file: nested_{i}.txt\n"));
	}
	repo.write("header_shaped.txt", &body);

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "header_shaped.txt".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let full_plan =
		plan_export(&selection, &Settings::default(), None).unwrap();
	let exact_escaped_len = full_plan.payload.len();

	let err = plan_export(
		&selection,
		&Settings::default(),
		Some(exact_escaped_len - 1),
	)
	.unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, exact_escaped_len - 1);
			assert_eq!(actual, exact_escaped_len);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}

	let ok_plan =
		plan_export(&selection, &Settings::default(), Some(exact_escaped_len))
			.unwrap();
	assert_eq!(ok_plan.payload.len(), exact_escaped_len);
}

// ---------------------------------------------------------------------------
// 31. Budget: oversize skip notices counted before allocation
// ---------------------------------------------------------------------------

#[test]
fn test_budget_oversize_skip_notices_counted_before_allocation() {
	let repo = TestRepo::new("skip-notice-budget");
	repo.write("big1.txt", &"A".repeat(2048));
	repo.write("big2.txt", &"B".repeat(2048));

	let settings = Settings {
		max_file_size_kb: 1.0,
		..Settings::default()
	};

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "big1.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "big2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.skipped_file_size_count, 2);
	assert!(plan.payload.contains("// File skipped: size exceeds limit"));

	let total_len = plan.payload.len();
	let err =
		plan_export(&selection, &settings, Some(total_len - 1)).unwrap_err();
	assert!(matches!(err, TransferError::PayloadLimitExceeded { .. }));
}

// ---------------------------------------------------------------------------
// 32. Deleted old content bypasses per-file limit subject to total cap
// ---------------------------------------------------------------------------

#[test]
fn test_deleted_old_content_bypasses_per_file_limit_subject_to_total_cap() {
	let repo = TestRepo::new("deleted-cap");
	repo.write("huge_deleted.txt", &"D".repeat(5000));
	let _ = repo.commit("add huge");
	repo.git(&["rm", "huge_deleted.txt"]);
	let del_commit = repo.commit("delete huge");

	let settings = Settings {
		max_file_size_kb: 1.0, // 1 KB per-file limit
		..Settings::default()
	};

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "huge_deleted.txt".to_string(),
			source: SourceKind::Commit { rev: del_commit },
			change_type: Some(ChangeType::Deleted),
			gitlink: false,
		}],
	)
	.unwrap();

	// 1. Without total budget, deleted old content bypasses the 1 KB per-file limit (accepted contract)
	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert!(plan.files[0].content.as_ref().unwrap().len() >= 5000);
	assert_eq!(plan.skipped_file_size_count, 0);

	// 2. But subject to NEW total budget cap: budget of 2000 bytes rejects the 5000-byte deleted content!
	let err = plan_export(&selection, &settings, Some(2000)).unwrap_err();
	match err {
		TransferError::PayloadLimitExceeded { limit, actual, .. } => {
			assert_eq!(limit, 2000);
			assert!(actual >= 5000);
		}
		other => panic!("expected PayloadLimitExceeded, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 33. Admitted FIFO rejected before open without blocking
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn test_admitted_fifo_rejected_without_blocking() {
	let repo = TestRepo::new("fifo-reject");
	let fifo_path = repo.path().join("test_fifo.pipe");
	let status = Command::new("mkfifo")
		.arg(&fifo_path)
		.status()
		.expect("mkfifo failed");
	assert!(status.success());

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "test_fifo.pipe".to_string(),
			source: SourceKind::Working,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	// If plan_export opened the FIFO without a writer, it would hang forever.
	// We run it on a background thread and join with a 2-second timeout channel.
	let (tx, rx) = std::sync::mpsc::channel();
	let handle = std::thread::spawn(move || {
		let res = plan_export(&selection, &Settings::default(), None);
		let _ = tx.send(res);
	});

	let res = rx
		// A blocked FIFO read never returns; 30s only rules out a busy host.
		.recv_timeout(std::time::Duration::from_secs(30))
		.expect("plan_export blocked on FIFO!");
	let _ = handle.join();
	let _ = fs::remove_file(&fifo_path);

	match res {
		Err(TransferError::SpecialFile(path)) => {
			assert!(path.contains("test_fifo.pipe"));
		}
		other => panic!("expected SpecialFile error, got: {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 34. Filtered FIFO is not read or rejected
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn test_filtered_fifo_not_read_or_rejected() {
	let repo = TestRepo::new("filtered-fifo");
	let fifo_path = repo.path().join("ignored.pipe");
	let status = Command::new("mkfifo")
		.arg(&fifo_path)
		.status()
		.expect("mkfifo failed");
	assert!(status.success());

	repo.write("regular.txt", "valid regular content");

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "ignored.pipe".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "regular.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let settings = Settings {
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Path,
			action: FilterAction::Exclude,
			value: "ignored.pipe".into(),
			enabled: true,
		}],
		..Settings::default()
	};

	// Should not block and should NOT error on the FIFO because it was excluded before reading!
	let (tx, rx) = std::sync::mpsc::channel();
	let handle = std::thread::spawn(move || {
		let res = plan_export(&selection, &settings, None);
		let _ = tx.send(res);
	});

	let res = rx
		// A blocked FIFO read never returns; 30s only rules out a busy host.
		.recv_timeout(std::time::Duration::from_secs(30))
		.expect("plan_export blocked on filtered FIFO!");
	let _ = handle.join();
	let _ = fs::remove_file(&fifo_path);

	let plan = res.expect("export should succeed for filtered FIFO");
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].path, "regular.txt");
	assert_eq!(
		plan.files[0].content.as_deref(),
		Some("valid regular content")
	);
	// Freshness should only have recorded regular.txt, never the FIFO!
	assert_eq!(plan.freshness.working_files.len(), 1);
	assert!(plan
		.freshness
		.working_files
		.contains_key(&(repo.canonical_id(), "regular.txt".to_string())));
}

// ---------------------------------------------------------------------------
// 35. Filtered oversized file is not read or hashed
// ---------------------------------------------------------------------------

#[test]
fn test_filtered_oversized_file_not_read_or_hashed() {
	let repo = TestRepo::new("filtered-oversize");
	repo.write("huge.bin", &"X".repeat(200_000));
	repo.write("ok.txt", "small text");

	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "huge.bin".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "ok.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let settings = Settings {
		max_file_size_kb: 10.0,
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Path,
			action: FilterAction::Exclude,
			value: "huge.bin".into(),
			enabled: true,
		}],
		..Settings::default()
	};

	let plan = plan_export(&selection, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].path, "ok.txt");
	assert_eq!(plan.skipped_file_size_count, 0); // huge.bin was filtered, not counted as skipped size!
	assert_eq!(plan.copied_file_count, 1);
	// Working files freshness only captures ok.txt!
	assert_eq!(plan.freshness.working_files.len(), 1);
	assert!(plan
		.freshness
		.working_files
		.contains_key(&(repo.canonical_id(), "ok.txt".to_string())));
}

// ---------------------------------------------------------------------------
// 36. Oracle: empty pre/post with only filtered staged entry preserves empty wrappers
// ---------------------------------------------------------------------------

#[test]
fn test_oracle_empty_pre_post_only_filtered_staged_entry() {
	let repo = TestRepo::new("staged-oracle");
	repo.write("tracked.ts", "content");
	repo.git(&["add", "tracked.ts"]);
	let _ = repo.commit("initial");

	repo.write("staged.ts", "staged content");
	repo.git(&["add", "staged.ts"]);

	let settings = Settings {
		pre_text: String::new(),
		post_text: String::new(),
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Path,
			action: FilterAction::Exclude,
			value: "staged.ts".into(),
			enabled: true,
		}],
		..Settings::default()
	};

	// Reference collector from gitsrc
	let git = Git::open(repo.path()).unwrap();
	let gitsrc_res = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Staged,
		&[repo.path().to_path_buf()],
		&settings,
		None,
		None,
	)
	.unwrap();

	// plan_export with Staged selection
	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "staged.ts".to_string(),
			source: SourceKind::Staged,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();

	// Exact byte parity: when the staged entry is filtered out, fallback is NOT triggered,
	// so empty wrappers behavior matches gitsrc byte-for-byte!
	assert_eq!(plan.payload, gitsrc_res.payload);
	assert_eq!(plan.files, gitsrc_res.files);
	assert_eq!(plan.copied_file_count, gitsrc_res.copied_file_count);
	assert_eq!(plan.file_limit_reached, gitsrc_res.file_limit_reached);
}

// ---------------------------------------------------------------------------
// 37. Oracle: File mode count limit trips on candidate before filtering
// ---------------------------------------------------------------------------

#[test]
fn test_oracle_file_count_limit_followed_by_excluded_candidate() {
	let repo = TestRepo::new("file-limit-oracle");
	repo.write("a.txt", "content a");
	repo.write("b.txt", "content b");

	let settings = Settings {
		set_max_file_count: true,
		file_count_limit: 1.0,
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Path,
			action: FilterAction::Exclude,
			value: "b.txt".into(),
			enabled: true,
		}],
		..Settings::default()
	};

	// Reference collector from copy.rs (File mode)
	let copy_res = copy::collect_copy_files(
		&[repo.path().to_path_buf()],
		&[repo.path().join("a.txt"), repo.path().join("b.txt")],
		&settings,
	);
	// In copy.rs, limit_hit is checked BEFORE filter, so b.txt trips the limit flag!
	assert!(copy_res.file_limit_reached);
	assert_eq!(copy_res.copied_file_count, 1);

	// plan_export with SourceKind::File
	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "a.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "b.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();

	// Exact parity: file_limit_reached is true in both!
	assert_eq!(plan.file_limit_reached, copy_res.file_limit_reached);
	assert_eq!(plan.copied_file_count, copy_res.copied_file_count);
	assert_eq!(plan.payload, copy_res.payload);
	assert_eq!(plan.files, copy_res.files);
}

// ---------------------------------------------------------------------------
// 38. Commit mode: empty selection omits source_root
// ---------------------------------------------------------------------------

#[test]
fn test_graph_commit_filtered_selection_omits_source_root() {
	let repo = TestRepo::new("commit-empty-root");
	repo.write("secret.env", "SECRET=1");
	let sha = repo.commit("initial");

	let settings = Settings {
		use_filters: true,
		use_exclude_filters: true,
		filter_rules: vec![FilterRule {
			kind: FilterType::Path,
			action: FilterAction::Exclude,
			value: "secret.env".into(),
			enabled: true,
		}],
		..Settings::default()
	};

	// Reference gitsrc graph copy
	let git = Git::open(repo.path()).unwrap();
	let gitsrc_res = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Commit(sha.clone()),
		&[repo.path().to_path_buf()],
		&settings,
		None,
		None,
	)
	.unwrap();

	// plan_export with Commit selection
	let selection = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		None,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "secret.env".to_string(),
			source: SourceKind::Commit { rev: sha },
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();

	// Graph empty payload omits source_root marker line!
	assert!(!plan.payload.contains("# source_root:"));
	assert_eq!(plan.payload, gitsrc_res.payload);
	assert_eq!(plan.files, gitsrc_res.files);
}

// ---------------------------------------------------------------------------
// Deleted working files: absence is part of the freshness snapshot
// ---------------------------------------------------------------------------

fn deleted_item(repo: &TestRepo, rel: &str, source: SourceKind) -> ExportItem {
	ExportItem {
		root: repo.canonical_id(),
		relative_path: rel.to_string(),
		source,
		change_type: Some(ChangeType::Deleted),
		gitlink: false,
	}
}

#[test]
fn test_working_deleted_absence_invalidates_export_when_recreated() {
	let src = TestRepo::new("src-repo");
	src.write("gone.txt", "old body\n");
	src.commit("c1");
	fs::remove_file(src.path().join("gone.txt")).unwrap();

	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![deleted_item(&src, "gone.txt", SourceKind::Working)],
	)
	.unwrap();
	let plan = plan_export(&selection, &Settings::default(), None).unwrap();
	assert!(plan.payload.contains("old body"));
	assert!(plan.revalidate().is_ok());

	// Recreated after planning: the "deletion" on the clipboard is stale.
	src.write("gone.txt", "recreated\n");
	match plan.revalidate().unwrap_err() {
		TransferError::StaleSource { reason, .. } => {
			assert!(reason.contains("gone.txt"), "{reason}");
		}
		other => panic!("expected StaleSource, got {other:?}"),
	}
	// Planning a deletion whose path exists is stale from the start.
	let err = plan_export(&selection, &Settings::default(), None).unwrap_err();
	assert!(matches!(err, TransferError::StaleSource { .. }), "{err:?}");
}

#[test]
fn test_deleted_sources_read_distinct_bases_and_match_legacy_working() {
	let src = TestRepo::new("src-repo");
	src.write("f.txt", "head body\n");
	src.write("g.txt", "g head body\n");
	src.commit("c1");
	// f: newer content staged, then deleted in the worktree only.
	src.write("f.txt", "index body\n");
	src.git(&["add", "f.txt"]);
	fs::remove_file(src.path().join("f.txt")).unwrap();
	let payload_for = |source: SourceKind, rel: &str| {
		let selection = ExportSelection::new(
			vec![src.path().to_path_buf()],
			None,
			vec![deleted_item(&src, rel, source)],
		)
		.unwrap();
		plan_export(&selection, &Settings::default(), None)
			.unwrap()
			.payload
	};

	// Unstaged compares the worktree with the index: the index had it.
	let unstaged = payload_for(SourceKind::Unstaged, "f.txt");
	assert!(unstaged.contains("index body") && !unstaged.contains("head body"));
	// Working is the SCM view and reads HEAD, byte-identical to gitsrc.
	let working = payload_for(SourceKind::Working, "f.txt");
	assert!(working.contains("head body") && !working.contains("index body"));
	let legacy = gitsrc::collect_payload(
		&src.open(),
		&GitSource::Working,
		&[src.path().to_path_buf()],
		&Settings::default(),
	)
	.unwrap();
	assert_eq!(working, legacy.payload);

	// A staged deletion is gone from the index; HEAD has the old body.
	src.git(&["rm", "-q", "g.txt"]);
	let staged = payload_for(SourceKind::Staged, "g.txt");
	assert!(staged.contains("g head body"), "{staged}");
}

#[test]
fn test_deleted_gitlink_pre_deletion_spec_is_not_a_blob_is_malformed() {
	let src = TestRepo::new("deleted-gitlink");
	src.write("a.txt", "base\n");
	let base = src.commit("base");

	// Add gitlink pointing to an existing commit in the superproject's object store.
	src.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},sub"),
	]);
	src.write("a.txt", "bump\n");
	src.git(&["add", "a.txt"]);
	src.git(&["commit", "-q", "-m", "add submodule"]);

	// Delete gitlink from index: now it is deleted in Working view (HEAD has it, index/worktree does not)
	src.git(&["update-index", "--force-remove", "sub"]);

	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![deleted_item(&src, "sub", SourceKind::Working)],
	)
	.unwrap();

	let err = plan_export(&selection, &Settings::default(), None).unwrap_err();
	match err {
		TransferError::Git(gitsrc::GitError::Malformed(msg)) => {
			assert!(msg.contains("not a file"), "unexpected message: {msg}");
		}
		other => panic!(
			"expected TransferError::Git(GitError::Malformed), got {other:?}"
		),
	}
}

#[test]
fn exact_commit_set_keeps_selected_tip_and_rejects_side_commits() {
	let repo = TestRepo::new("exact-commits");
	repo.write("root.txt", "root\n");
	let root = repo.commit("root");
	repo.write("main.txt", "main\n");
	let _main_mid = repo.commit("on main");
	repo.git(&["checkout", "-q", "-b", "side", &root]);
	repo.write("side.txt", "side\n");
	let side = repo.commit("on side");
	repo.git(&["checkout", "-q", "main"]);
	repo.write("main2.txt", "main2\n");
	let main_tip = repo.commit("main tip");
	let git = repo.open();

	let err = plan_commit_export_exact(
		&git,
		&main_tip,
		&[main_tip.clone(), side.clone(), root.clone()],
	)
	.unwrap_err();
	assert!(
		matches!(err, TransferError::DiscontinuousCommits { .. }),
		"{err}"
	);

	let skipped = plan_commit_export_exact(
		&git,
		&main_tip,
		&[main_tip.clone(), root.clone()],
	)
	.unwrap_err();
	assert!(
		matches!(skipped, TransferError::DiscontinuousCommits { .. }),
		"{skipped}"
	);

	let payload =
		plan_commit_export_exact(&git, &side, &[side.clone(), root.clone()])
			.unwrap();
	assert_eq!(payload.commits.len(), 2);
	assert!(payload.commits[0].message.starts_with("root"));
	assert!(payload.commits[1].message.starts_with("on side"));
	assert!(payload
		.commits
		.iter()
		.any(|c| { c.files.iter().any(|f| f.path == "side.txt") }));
	assert!(!payload
		.commits
		.iter()
		.any(|c| { c.files.iter().any(|f| f.path == "main2.txt") }));

	let only_root =
		plan_commit_export_exact(&git, &root, std::slice::from_ref(&root))
			.unwrap();
	assert_eq!(only_root.commits.len(), 1);
	assert!(only_root.commits[0].message.starts_with("root"));

	let head_last = plan_commit_export(&git, None, Some(2)).unwrap();
	assert!(head_last.commits.iter().any(|c| {
		c.message.starts_with("main tip")
			|| c.files.iter().any(|f| f.path == "main2.txt")
	}));
	assert_ne!(
		payload.commits.last().unwrap().message,
		head_last.commits.last().unwrap().message
	);
}

#[test]
fn commit_replay_preview_rejects_head_index_content_and_absence_changes() {
	use snip_core::commits::{
		self, CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	let repo = TestRepo::new("replay-fresh");
	repo.write("a.txt", "base\n");
	repo.commit("base");
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
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	preview.revalidate().unwrap();

	repo.write("a.txt", "external change\n");
	assert!(preview.revalidate().is_err());
	assert_eq!(
		fs::read_to_string(repo.path().join("a.txt")).unwrap(),
		"external change\n"
	);

	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	repo.git(&["add", "a.txt"]);
	assert!(
		preview.revalidate().is_err(),
		"index change after preview must be stale"
	);

	let create = CommitsPayload {
		commits: vec![CommitRecord {
			message: "add b\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "b.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("new\n".into()),
				not_copied: None,
			}],
		}],
	};
	let absent = CommitReplayPreview::capture(repo.path(), &create).unwrap();
	repo.write("b.txt", "appeared\n");
	assert!(absent.revalidate().is_err());
	assert!(!commits::to_clipboard_text(&create).is_empty());
}

#[test]
fn test_broken_index_is_an_error_not_a_deleted_marker() {
	let src = TestRepo::new("src-repo");
	src.write("f.txt", "body\n");
	src.commit("c1");
	fs::remove_file(src.path().join("f.txt")).unwrap();
	let index = src.path().join(".git/index");
	fs::write(&index, b"DIRC garbage that is not an index").unwrap();

	let selection = ExportSelection::new(
		vec![src.path().to_path_buf()],
		None,
		vec![deleted_item(&src, "f.txt", SourceKind::Unstaged)],
	)
	.unwrap();
	let err = plan_export(&selection, &Settings::default(), None).unwrap_err();
	assert!(matches!(err, TransferError::Git(_)), "{err:?}");
}

#[test]
fn skipped_non_utf8_replay_target_becomes_stale_when_it_turns_writable() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange, ReplayAction,
		ReplaySkipReason,
	};

	let repo = TestRepo::new("skip-fresh");
	repo.write("a.txt", "base\n");
	repo.commit("base");
	let head = repo.git(&["rev-parse", "HEAD"]);
	let index = repo.git(&["rev-parse", ":a.txt"]);
	fs::write(repo.path().join("a.txt"), [0xff, 0xfe, 0x01]).unwrap();
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
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	assert_eq!(
		preview.plan().commits[0].files[0].action,
		ReplayAction::Skip
	);
	assert_eq!(
		preview.plan().commits[0].files[0].skip_reason,
		Some(ReplaySkipReason::NonUtf8Target)
	);
	assert!(
		preview
			.freshness()
			.target_files
			.keys()
			.any(|path| path.ends_with("a.txt")),
		"the skipped file has to be in the freshness snapshot"
	);
	preview.revalidate().unwrap();

	fs::write(repo.path().join("a.txt"), "external change\n").unwrap();
	let err = preview.revalidate().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err}"
	);
	assert_eq!(
		fs::read(repo.path().join("a.txt")).unwrap(),
		b"external change\n"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
	assert_eq!(repo.git(&["rev-parse", ":a.txt"]), index);

	// A payload that was not copied stays skipped when the destination changes.
	let not_copied = CommitsPayload {
		commits: vec![CommitRecord {
			message: "skip source\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: None,
				not_copied: Some(snip_core::commits::NotCopiedReason::Binary),
			}],
		}],
	};
	let skipped =
		CommitReplayPreview::capture(repo.path(), &not_copied).unwrap();
	assert_eq!(
		skipped.plan().commits[0].files[0].skip_reason,
		Some(ReplaySkipReason::NotCopied)
	);
	fs::write(repo.path().join("a.txt"), "still external\n").unwrap();
	skipped.revalidate().unwrap();
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn commit_replay_preview_treats_a_file_in_the_way_as_an_unsafe_skip() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange, ReplaySkipReason,
	};

	let repo = TestRepo::new("file-in-the-way");
	repo.write("newdir", "i am a file\n");
	repo.write("ok.txt", "ok\n");
	repo.commit("base");
	let file = |path: &str| CommitFile {
		path: path.into(),
		old_path: None,
		change: FileChange::Added,
		content: Some("x\n".into()),
		not_copied: None,
	};
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![
				file("newdir/x.txt"),
				file("newdir/a/b.txt"),
				file("fresh.txt"),
			],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload)
		.expect("a file standing in for a directory must not refuse the paste");
	let files = &preview.plan().commits[0].files;
	assert_eq!(files[0].skip_reason, Some(ReplaySkipReason::UnsafePath));
	assert_eq!(files[1].skip_reason, Some(ReplaySkipReason::UnsafePath));
	assert_eq!(files[2].skip_reason, None);
	assert_eq!(
		fs::read(repo.path().join("newdir")).unwrap(),
		b"i am a file\n"
	);
}

#[cfg(unix)]
#[test]
fn unsafe_symlink_replay_parent_is_not_followed_and_becomes_stale() {
	let repo = TestRepo::new("skip-link");
	repo.write("a.txt", "base\n");
	repo.commit("base");
	let outside = repo.path().parent().unwrap().join("outside-secret");
	fs::write(&outside, b"secret-bytes").unwrap();
	std::os::unix::fs::symlink(&outside, repo.path().join("link")).unwrap();
	let payload = snip_core::commits::CommitsPayload {
		commits: vec![snip_core::commits::CommitRecord {
			message: "through link\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![snip_core::commits::CommitFile {
				path: "link/a.txt".into(),
				old_path: None,
				change: snip_core::commits::FileChange::Added,
				content: Some("incoming\n".into()),
				not_copied: None,
			}],
		}],
	};
	let before = fs::read(&outside).unwrap();
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	assert_eq!(fs::read(&outside).unwrap(), before);
	assert_eq!(
		preview.plan().commits[0].files[0].skip_reason,
		Some(snip_core::commits::ReplaySkipReason::UnsafePath)
	);
	assert!(preview.plan().commits[0].files[0].absolute_path.is_none());
	assert!(preview.freshness().target_files.is_empty());
	fs::remove_file(repo.path().join("link")).unwrap();
	fs::create_dir(repo.path().join("link")).unwrap();
	fs::write(repo.path().join("link/a.txt"), "external\n").unwrap();
	assert!(preview.revalidate().is_err());
	assert_eq!(
		fs::read(repo.path().join("link/a.txt")).unwrap(),
		b"external\n"
	);
	assert_eq!(fs::read(&outside).unwrap(), before);
}

// ---------------------------------------------------------------------------
// 26. A plain (non-Git) folder exports File items byte-identically to a repo
// ---------------------------------------------------------------------------

#[test]
fn test_plain_folder_file_export_matches_a_repo() {
	let repo = TestRepo::new("proj");
	repo.write("notes/readme.txt", "PLAIN_BYTES\n");
	let plain_dir = tempfile::tempdir().unwrap();
	let plain = dunce::canonicalize(plain_dir.path()).unwrap().join("proj");
	fs::create_dir_all(plain.join("notes")).unwrap();
	fs::write(plain.join("notes/readme.txt"), "PLAIN_BYTES\n").unwrap();
	assert!(
		Git::open(&plain).is_err(),
		"the plain folder must not be a repo"
	);

	let export = |root: &Path| {
		let selection = ExportSelection::new(
			vec![root.to_path_buf()],
			Some(root.to_path_buf()),
			vec![ExportItem {
				root: CanonicalRootId::new(root).unwrap(),
				relative_path: "notes/readme.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			}],
		)
		.unwrap();
		let plan = plan_export(&selection, &Settings::default(), None).unwrap();
		plan.revalidate().unwrap();
		plan.payload
	};
	let from_plain = export(&plain);
	assert!(from_plain.contains("PLAIN_BYTES"), "{from_plain}");
	assert_eq!(from_plain, export(repo.path()));
}

#[test]
fn commit_replay_apply_replays_the_previewed_payload() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	let repo = TestRepo::new("replay-apply");
	repo.write("base.txt", "base\n");
	repo.commit("base");

	// 事先 stage 的無關檔案
	repo.write("staged.txt", "staged by user\n");
	repo.git(&["add", "staged.txt"]);

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "applied commit message\n".into(),
			author_name: "Apply Author".into(),
			author_email: "apply@example.invalid".into(),
			author_date: "2026-09-25T12:00:00Z".into(),
			files: vec![CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("payload bytes\n".into()),
				not_copied: None,
			}],
		}],
	};

	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	let result = preview.apply().unwrap();
	assert_eq!(result.failure, None);
	assert_eq!(result.created.len(), 1);

	// `git show HEAD:a.txt` 的 bytes 等於 payload
	assert_eq!(repo.git(&["show", "HEAD:a.txt"]), "payload bytes");
	assert_eq!(
		fs::read(repo.path().join("a.txt")).unwrap(),
		b"payload bytes\n"
	);

	// `log -1 --format=%an|%ae|%aI|%B` 等於 payload
	let meta = repo.git(&["log", "-1", "--format=%an|%ae|%aI|%B"]);
	let expected_meta = format!(
		"{}|{}|{}|{}",
		payload.commits[0].author_name,
		payload.commits[0].author_email,
		payload.commits[0].author_date,
		payload.commits[0].message
	)
	.trim()
	.to_string();
	// 不同 git 版本把 UTC 印成 Z 或 +00:00，先正規化
	assert_eq!(meta.replace("+00:00", "Z"), expected_meta);

	// 事先 stage 的無關檔案仍在 `diff --cached --name-only` 裡
	let staged = repo.git(&["diff", "--cached", "--name-only"]);
	assert!(staged.lines().any(|l| l == "staged.txt"));
}

#[test]
fn commit_replay_apply_refuses_a_destination_changed_after_preview() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	let repo = TestRepo::new("replay-stale-destination");
	repo.write("a.txt", "base a\n");
	repo.commit("base");

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "mod a\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: Some("incoming a\n".into()),
				not_copied: None,
			}],
		}],
	};

	// 段落 1：外部改寫 a.txt
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	let head_before = repo.git(&["rev-parse", "HEAD"]);
	let index_before = repo.git(&["rev-parse", ":a.txt"]);
	let count_before = repo.git(&["rev-list", "--count", "HEAD"]);
	repo.write("a.txt", "external change a\n");
	let bytes_before = fs::read(repo.path().join("a.txt")).unwrap();
	let err = preview.apply().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err:?}"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
	assert_eq!(repo.git(&["rev-parse", ":a.txt"]), index_before);
	assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), count_before);
	assert_eq!(fs::read(repo.path().join("a.txt")).unwrap(), bytes_before);

	// 段落 2：外部 git add
	repo.write("a.txt", "base a\n");
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	repo.write("a.txt", "staged a\n");
	repo.git(&["add", "a.txt"]);
	let head_before = repo.git(&["rev-parse", "HEAD"]);
	let index_before = repo.git(&["rev-parse", ":a.txt"]);
	let count_before = repo.git(&["rev-list", "--count", "HEAD"]);
	let bytes_before = fs::read(repo.path().join("a.txt")).unwrap();
	let err = preview.apply().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err:?}"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
	assert_eq!(repo.git(&["rev-parse", ":a.txt"]), index_before);
	assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), count_before);
	assert_eq!(fs::read(repo.path().join("a.txt")).unwrap(), bytes_before);

	// 段落 3：原本不存在的 b.txt 出現
	repo.git(&["checkout", "--", "a.txt"]);
	let payload_b = CommitsPayload {
		commits: vec![CommitRecord {
			message: "add b\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "b.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("incoming b\n".into()),
				not_copied: None,
			}],
		}],
	};
	let preview =
		CommitReplayPreview::capture(repo.path(), &payload_b).unwrap();
	let head_before = repo.git(&["rev-parse", "HEAD"]);
	let index_before = repo.git(&["rev-parse", ":a.txt"]);
	let count_before = repo.git(&["rev-list", "--count", "HEAD"]);
	let bytes_a_before = fs::read(repo.path().join("a.txt")).unwrap();
	repo.write("b.txt", "external b appeared\n");
	let bytes_b_before = fs::read(repo.path().join("b.txt")).unwrap();
	let err = preview.apply().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err:?}"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
	assert_eq!(repo.git(&["rev-parse", ":a.txt"]), index_before);
	assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), count_before);
	assert_eq!(fs::read(repo.path().join("a.txt")).unwrap(), bytes_a_before);
	assert_eq!(fs::read(repo.path().join("b.txt")).unwrap(), bytes_b_before);
}

#[test]
fn commit_replay_apply_refuses_after_head_moved() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	let repo = TestRepo::new("replay-head-moved");
	repo.write("base.txt", "base\n");
	repo.commit("base");

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "incoming.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("incoming\n".into()),
				not_copied: None,
			}],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();

	repo.write("other.txt", "other\n");
	let external_head = repo.commit("external commit");
	let count_before = repo.git(&["rev-list", "--count", "HEAD"]);
	let index_before = repo.git(&["ls-files", "-s"]);

	let err = preview.apply().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err:?}"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), external_head);
	assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), count_before);
	assert_eq!(repo.git(&["ls-files", "-s"]), index_before);
	assert!(!repo.exists("incoming.txt"));
}

#[cfg(unix)]
#[test]
fn commit_replay_apply_through_a_symlinked_root() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	let repo = TestRepo::new("symlink-root");
	repo.write("base.txt", "base\n");
	let base_head = repo.commit("base");

	let link_dir = tempfile::tempdir().unwrap();
	let link_path = link_dir.path().join("link");
	std::os::unix::fs::symlink(repo.path(), &link_path).unwrap();

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "symlink commit\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("content in a\n".into()),
				not_copied: None,
			}],
		}],
	};

	// 用 link 路徑 capture 後 apply 成功，實際 repo 的 HEAD 前進
	let preview = CommitReplayPreview::capture(&link_path, &payload).unwrap();
	let result = preview.apply().unwrap();
	assert_eq!(result.failure, None);
	assert_eq!(result.created.len(), 1);
	let new_head = repo.git(&["rev-parse", "HEAD"]);
	assert_ne!(new_head, base_head);
	assert_eq!(result.created[0], new_head);

	// 再用 link capture 一次，從實際路徑改寫目標檔，apply 回 StaleDestination
	let payload2 = CommitsPayload {
		commits: vec![CommitRecord {
			message: "second commit\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: Some("content v2\n".into()),
				not_copied: None,
			}],
		}],
	};
	let preview2 = CommitReplayPreview::capture(&link_path, &payload2).unwrap();
	fs::write(repo.path().join("a.txt"), "external change\n").unwrap();
	let err = preview2.apply().unwrap_err();
	assert!(
		matches!(err, TransferError::StaleDestination { .. }),
		"{err:?}"
	);
}

#[test]
fn commit_replay_apply_leaves_a_not_copied_target_alone() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange, NotCopiedReason,
	};

	let repo = TestRepo::new("not-copied-target");
	repo.write("bin.dat", "original binary content\n");
	repo.commit("base");

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "skip binary and add text\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![
				CommitFile {
					path: "bin.dat".into(),
					old_path: None,
					change: FileChange::Modified,
					content: None,
					not_copied: Some(NotCopiedReason::Binary),
				},
				CommitFile {
					path: "added.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("added\n".into()),
					not_copied: None,
				},
			],
		}],
	};

	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();
	// capture 後外部改寫 not-copied 的目標
	fs::write(repo.path().join("bin.dat"), "externally modified binary\n")
		.unwrap();

	let result = preview.apply().unwrap();
	assert_eq!(result.failure, None);
	assert_eq!(result.created.len(), 1);
	assert_eq!(
		fs::read(repo.path().join("bin.dat")).unwrap(),
		b"externally modified binary\n"
	);
	assert_eq!(fs::read(repo.path().join("added.txt")).unwrap(), b"added\n");
}

#[test]
fn commit_replay_apply_waits_for_the_worktree_lock() {
	use snip_core::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};
	use snip_core::gitrun::RunOptions;
	use snip_core::gitsrc::Git;
	use snip_core::workspace::{lock_heavy, RepoIdentity};
	use std::sync::mpsc;
	use std::time::Duration;

	let repo = TestRepo::new("worktree-lock");
	repo.write("base.txt", "base\n");
	let head_before = repo.commit("base");
	let count_before = repo.git(&["rev-list", "--count", "HEAD"]);
	let index_before = repo.git(&["ls-files", "-s"]);

	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "new commit\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![
				CommitFile {
					path: "base.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("incoming base\n".into()),
					not_copied: None,
				},
				CommitFile {
					path: "new.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("new content\n".into()),
					not_copied: None,
				},
			],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload).unwrap();

	let git = Git::open(repo.path()).unwrap();
	let id = RepoIdentity::resolve(&git, &RunOptions::default()).unwrap();
	let guard = lock_heavy(&id, &RunOptions::default()).unwrap();

	let opts = RunOptions {
		queue_timeout: Duration::from_secs(30),
		..RunOptions::default()
	};

	let (tx, rx) = mpsc::channel();
	let res = std::thread::scope(|s| {
		s.spawn(|| {
			let res = preview.apply_with(&opts);
			let _ = tx.send(res);
		});

		std::thread::sleep(Duration::from_millis(300));
		assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

		repo.write("base.txt", "modified base\n");

		drop(guard);

		rx.recv_timeout(Duration::from_secs(30))
			.expect("apply_with did not return after worktree lock released")
	});

	assert!(
		matches!(res, Err(TransferError::StaleDestination { .. })),
		"expected StaleDestination, got {res:?}"
	);
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
	assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), count_before);
	assert_eq!(repo.git(&["ls-files", "-s"]), index_before);
	assert_eq!(repo.read("base.txt"), "modified base\n");
	assert!(!repo.exists("new.txt"));
}

// ---------------------------------------------------------------------------
// expand_folder_items unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_expand_folder_items_git_and_nested_repo_pruning() {
	let repo = TestRepo::new("folder-pruning");
	repo.write("dir/normal.txt", "normal content");
	repo.write("dir/sub/other.txt", "sub content");
	repo.write("dir/nested/repo_file.txt", "nested content");
	// Nested repository with .git directory
	fs::create_dir_all(repo.path().join("dir/nested/.git")).unwrap();
	// Submodule / gitlink with .git file
	repo.write("dir/submodule/.git", "gitdir: ../../.git/modules/submodule");
	repo.write("dir/submodule/sub_file.txt", "submodule file");

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let expanded = expand_folder_items(sel, 100, &CancelToken::new()).unwrap();
	let mut rels: Vec<_> = expanded
		.sel
		.items
		.iter()
		.map(|item| item.relative_path.as_str())
		.collect();
	rels.sort();

	assert_eq!(rels, ["dir/normal.txt", "dir/sub/other.txt"]);
	assert!(!expanded.truncated);
	assert_eq!(expanded.skipped, 0);
}

#[test]
#[cfg(unix)]
fn test_expand_folder_items_fifo_skipped() {
	let repo = TestRepo::new("folder-fifo");
	repo.write("dir/normal.txt", "normal content");
	let fifo_path = repo.path().join("dir/named_pipe");
	let status = Command::new("mkfifo").arg(&fifo_path).status();
	match status {
		Ok(s) if s.success() => {}
		_ => {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"mkfifo command failed or is unavailable with SNIP_REQUIRE_ALL_TESTS set"
			);
			return;
		}
	}

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let (tx, rx) = std::sync::mpsc::channel();
	std::thread::spawn(move || {
		let res = expand_folder_items(sel, 100, &CancelToken::new());
		let _ = tx.send(res);
	});

	let expanded = rx
		.recv_timeout(std::time::Duration::from_secs(5))
		.expect("expand_folder_items timed out or blocked on FIFO")
		.expect("expand_folder_items returned error");

	assert!(expanded.skipped >= 1);
	assert!(expanded
		.sel
		.items
		.iter()
		.all(|item| item.relative_path != "dir/named_pipe"));
	assert_eq!(expanded.sel.items.len(), 1);
	assert_eq!(expanded.sel.items[0].relative_path, "dir/normal.txt");
}

#[test]
#[cfg(unix)]
fn test_expand_folder_items_symlink_boundaries() {
	let repo = TestRepo::new("folder-symlinks");
	repo.write("dir/normal.txt", "normal content");
	repo.write("dir/target.txt", "target content");

	// In-root symlink pointing to an existing file
	std::os::unix::fs::symlink(
		repo.path().join("dir/target.txt"),
		repo.path().join("dir/in_root.txt"),
	)
	.unwrap();

	// Out-of-root symlink pointing to a file outside the repository
	let outside = tempfile::tempdir().unwrap();
	let outside_file = outside.path().join("secret.txt");
	fs::write(&outside_file, "secret").unwrap();
	std::os::unix::fs::symlink(
		&outside_file,
		repo.path().join("dir/outside.txt"),
	)
	.unwrap();

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let expanded = expand_folder_items(sel, 100, &CancelToken::new()).unwrap();
	let mut rels: Vec<_> = expanded
		.sel
		.items
		.iter()
		.map(|item| item.relative_path.as_str())
		.collect();
	rels.sort();

	assert_eq!(
		rels,
		["dir/in_root.txt", "dir/normal.txt", "dir/target.txt"]
	);
	assert_eq!(expanded.skipped, 1);
	assert!(!expanded.truncated);
}

#[test]
fn test_expand_folder_items_budget_truncation_and_deduplication() {
	let repo = TestRepo::new("folder-budget");
	for i in 1..=5 {
		repo.write(&format!("dir/f{i}.txt"), &format!("content {i}"));
	}

	// 1. limit = 2 with 5 files -> 2 file items, truncated == true
	let sel1 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();
	let expanded1 = expand_folder_items(sel1, 2, &CancelToken::new()).unwrap();
	assert_eq!(expanded1.sel.items.len(), 2);
	assert!(expanded1.truncated);

	// 2. Picked (non-folder) items reduce the budget
	repo.write("picked.txt", "picked content");
	let sel2 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "picked.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "dir".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();
	let expanded2 = expand_folder_items(sel2, 2, &CancelToken::new()).unwrap();
	assert_eq!(expanded2.sel.items.len(), 2);
	assert!(expanded2.truncated);
	assert_eq!(expanded2.sel.items[0].relative_path, "picked.txt");
	assert!(expanded2.sel.items[1].relative_path.starts_with("dir/"));

	// 3. An already-selected file is not duplicated
	let sel3 = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "dir/f1.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "dir".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();
	let expanded3 = expand_folder_items(sel3, 10, &CancelToken::new()).unwrap();
	assert_eq!(expanded3.sel.items.len(), 5);
	let f1_count = expanded3
		.sel
		.items
		.iter()
		.filter(|item| item.relative_path == "dir/f1.txt")
		.count();
	assert_eq!(f1_count, 1);
	assert!(!expanded3.truncated);
}

#[test]
fn test_expand_folder_items_cancelled_token() {
	let repo = TestRepo::new("folder-cancel");
	repo.write("dir/a.txt", "a");
	repo.write("dir/b.txt", "b");
	repo.write("picked.txt", "picked");

	let token = CancelToken::new();
	token.cancel();

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "picked.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "dir".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		],
	)
	.unwrap();

	let expanded = expand_folder_items(sel, 100, &token).unwrap();
	assert_eq!(expanded.sel.items.len(), 1);
	assert_eq!(expanded.sel.items[0].relative_path, "picked.txt");
}

#[test]
#[cfg(unix)]
fn test_expand_folder_items_unrepresentable_names_skipped() {
	let repo = TestRepo::new("folder-unrepresentable");
	repo.write("dir/good.txt", "good");
	repo.write("dir/bad:name.txt", "bad colon");
	repo.write("dir/trailing.txt ", "trailing space");

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let expanded = expand_folder_items(sel, 100, &CancelToken::new()).unwrap();
	let rels: Vec<_> = expanded
		.sel
		.items
		.iter()
		.map(|item| item.relative_path.as_str())
		.collect();
	assert_eq!(rels, ["dir/good.txt"]);
	assert_eq!(expanded.skipped, 2);
	assert!(!expanded.truncated);
}

#[test]
#[cfg(unix)]
fn test_expand_folder_items_root_through_symlink() {
	let repo = TestRepo::new("folder-symlink-root");
	repo.write("dir/sub/file.txt", "content");

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	let root_id = CanonicalRootId::new(&alias_path).unwrap();
	let sel = ExportSelection::new(
		vec![alias_path.clone()],
		Some(alias_path),
		vec![ExportItem {
			root: root_id,
			relative_path: "dir".to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		}],
	)
	.unwrap();

	let expanded = expand_folder_items(sel, 100, &CancelToken::new()).unwrap();
	assert_eq!(expanded.sel.items.len(), 1);
	assert_eq!(expanded.sel.items[0].relative_path, "dir/sub/file.txt");
	assert_eq!(expanded.skipped, 0);
	assert!(!expanded.truncated);
}

// ---------------------------------------------------------------------------
// 29. Phase 2: Range oracle byte equality, changes listing, and parity
// ---------------------------------------------------------------------------

#[test]
fn test_range_oracle_byte_equality() {
	let repo = TestRepo::new("range-oracle");
	repo.write("modified.txt", "initial text\n");
	repo.write("deleted.txt", "content only at A\n");
	repo.write("rename_src.txt", "content of renamed\n");
	fs::write(repo.path().join("binary.bin"), [0u8, 1, 2, 3, 255, 0, 4])
		.unwrap();
	fs::write(repo.path().join("deleted_binary.bin"), [0u8, 9, 8, 7, 0])
		.unwrap();
	repo.write("twice.txt", "v1\n");
	let large_content = "x".repeat(15 * 1024);
	repo.write("large.txt", &large_content);
	let sha_a = repo.commit("commit A");

	// Intermediate commit to change twice.txt twice
	repo.write("twice.txt", "v2\n");
	repo.commit("commit intermediate");

	// Tip commit B
	repo.write("modified.txt", "updated text\n");
	repo.write("added.txt", "new file in B\n");
	repo.git(&["rm", "deleted.txt"]);
	repo.git(&["mv", "rename_src.txt", "rename_dst.txt"]);
	fs::write(repo.path().join("binary.bin"), [0u8, 1, 2, 3, 255, 0, 5])
		.unwrap();
	repo.git(&["rm", "deleted_binary.bin"]);
	repo.write("twice.txt", "v3\n");
	repo.write("large.txt", &format!("{large_content}extra\n"));
	let sha_b = repo.commit("commit B");

	let settings = Settings {
		max_file_size_kb: 10.0,
		..Settings::default()
	};
	let git = repo.open();
	let range_source = GitSource::Range(sha_a.clone(), sha_b.clone());

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();

	// Check items order and change types match list_changed_paths_with exactly
	let expected_paths = gitsrc::list_changed_paths_with(
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();
	let actual_paths: Vec<_> = changed
		.items
		.iter()
		.map(|item| (item.relative_path.clone(), item.change_type))
		.collect();
	assert_eq!(actual_paths, expected_paths);

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	let legacy =
		gitsrc::collect_payload(&git, &range_source, &[repo.path()], &settings)
			.unwrap();

	assert_eq!(plan.payload, legacy.payload);
	assert_eq!(plan.copied_file_count, legacy.copied_file_count);
	assert_eq!(plan.skipped_file_size_count, legacy.skipped_file_size_count);
	assert_eq!(
		plan.skipped_unreadable_count,
		legacy.skipped_unreadable_count
	);
}

#[test]
fn test_range_deletion_only_reads_base() {
	let repo = TestRepo::new("range-del-only");
	repo.write("del.txt", "content at base A\n");
	repo.write("other.txt", "other\n");
	let sha_a = repo.commit("commit A");

	repo.write("other.txt", "other intermediate\n");
	let _ = repo.commit("commit intermediate");

	repo.git(&["rm", "del.txt"]);
	repo.write("other.txt", "other\n");
	let sha_b = repo.commit("commit B");

	let settings = Settings::default();
	let git = repo.open();
	let range_source = GitSource::Range(sha_a.clone(), sha_b.clone());

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();
	assert_eq!(changed.items.len(), 1);
	assert_eq!(changed.items[0].relative_path, "del.txt");

	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(
		plan.files[0].content.as_deref(),
		Some("content at base A\n")
	);

	let legacy =
		gitsrc::collect_payload(&git, &range_source, &[repo.path()], &settings)
			.unwrap();
	assert_eq!(plan.payload, legacy.payload);
}

#[test]
fn test_range_empty_diff() {
	let repo = TestRepo::new("range-empty");
	repo.write("a.txt", "hello\n");
	let sha_a = repo.commit("commit A");

	let settings = Settings::default();
	let git = repo.open();
	let range_source = GitSource::Range(sha_a.clone(), sha_a.clone());

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();
	assert!(changed.items.is_empty());

	let legacy =
		gitsrc::collect_payload(&git, &range_source, &[repo.path()], &settings)
			.unwrap();
	assert!(legacy.files.is_empty());
}

#[test]
#[cfg(unix)]
fn test_range_oracle_root_through_symlink() {
	let repo = TestRepo::new("range-symlink-root");
	repo.write("mod.txt", "a\n");
	repo.write("del.txt", "del\n");
	let sha_a = repo.commit("A");

	repo.write("mod.txt", "b\n");
	repo.write("add.txt", "add\n");
	repo.git(&["rm", "del.txt"]);
	let sha_b = repo.commit("B");

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	let root_id = CanonicalRootId::new(&alias_path).unwrap();
	let git = Git::open(&alias_path).unwrap();
	let range_source = GitSource::Range(sha_a, sha_b);
	let settings = Settings::default();

	let changed =
		changed_items(&root_id, &git, &range_source, &RunOptions::default())
			.unwrap();
	let sel = ExportSelection::new(
		vec![alias_path.clone()],
		Some(alias_path),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	let legacy =
		gitsrc::collect_payload(&git, &range_source, &[repo.path()], &settings)
			.unwrap();
	assert_eq!(plan.payload, legacy.payload);
	assert_eq!(plan.copied_file_count, legacy.copied_file_count);
}

// ---------------------------------------------------------------------------
// 30. Phase 2: Working, Staged, and Commit parity with gitsrc
// ---------------------------------------------------------------------------

#[test]
fn test_working_oracle_parity_and_no_staged_working_conflict() {
	let repo = TestRepo::new("working-parity");
	repo.write("mod_unstaged.txt", "base mod unstaged\n");
	repo.write("staged_mod.txt", "base staged mod\n");
	repo.write("del_unstaged.txt", "base del unstaged\n");
	repo.write("del_staged.txt", "base del staged\n");
	repo.write("both_staged_and_mod.txt", "base both\n");
	repo.commit("base");

	// Working state:
	// 1. modified unstaged
	repo.write("mod_unstaged.txt", "worktree edit\n");
	// 2. untracked
	repo.write("untracked.txt", "untracked content\n");
	// 3. staged new
	repo.write("staged_new.txt", "staged new content\n");
	repo.git(&["add", "staged_new.txt"]);
	// 4. staged modified
	repo.write("staged_mod.txt", "staged mod content\n");
	repo.git(&["add", "staged_mod.txt"]);
	// 5. deleted unstaged
	fs::remove_file(repo.path().join("del_unstaged.txt")).unwrap();
	// 6. deleted staged
	repo.git(&["rm", "del_staged.txt"]);
	// 7. both staged and modified on disk
	repo.write("both_staged_and_mod.txt", "staged content\n");
	repo.git(&["add", "both_staged_and_mod.txt"]);
	repo.write("both_staged_and_mod.txt", "worktree further edit\n");

	let settings = Settings::default();
	let git = repo.open();
	let source = GitSource::Working;

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&source,
		&RunOptions::default(),
	)
	.unwrap();

	// All items must be SourceKind::Working, never mixed with Staged
	for item in &changed.items {
		assert_eq!(item.source, SourceKind::Working);
	}

	// Paths + change types match list_changed_paths_with order exactly
	let expected_paths =
		gitsrc::list_changed_paths_with(&git, &source, &RunOptions::default())
			.unwrap();
	let actual_paths: Vec<_> = changed
		.items
		.iter()
		.map(|item| (item.relative_path.clone(), item.change_type))
		.collect();
	assert_eq!(actual_paths, expected_paths);

	// ExportSelection succeeds without StagedWorkingConflict
	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	let legacy =
		gitsrc::collect_payload(&git, &source, &[repo.path()], &settings)
			.unwrap();

	assert_eq!(plan.payload, legacy.payload);
	assert_eq!(plan.copied_file_count, legacy.copied_file_count);
	assert_eq!(plan.skipped_file_size_count, legacy.skipped_file_size_count);
	assert_eq!(
		plan.skipped_unreadable_count,
		legacy.skipped_unreadable_count
	);
}

#[test]
#[cfg(unix)]
fn test_working_oracle_root_through_symlink() {
	let repo = TestRepo::new("working-symlink-root");
	repo.write("f1.txt", "init\n");
	repo.commit("base");

	repo.write("f1.txt", "changed\n");
	repo.write("untracked.txt", "new\n");

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	let root_id = CanonicalRootId::new(&alias_path).unwrap();
	let git = Git::open(&alias_path).unwrap();
	let source = GitSource::Working;
	let settings = Settings::default();

	let changed =
		changed_items(&root_id, &git, &source, &RunOptions::default()).unwrap();
	let sel = ExportSelection::new(
		vec![alias_path.clone()],
		Some(alias_path),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	let legacy =
		gitsrc::collect_payload(&git, &source, &[repo.path()], &settings)
			.unwrap();
	assert_eq!(plan.payload, legacy.payload);
}

#[test]
fn test_staged_and_commit_oracle_parity() {
	let repo = TestRepo::new("staged-commit-parity");
	repo.write("f1.txt", "base f1\n");
	repo.write("del.txt", "del content\n");
	let c1 = repo.commit("base commit");

	// Commit 2 with changes
	repo.write("f1.txt", "c2 f1\n");
	repo.write("added.txt", "added in c2\n");
	repo.git(&["rm", "del.txt"]);
	let c2 = repo.commit("c2 commit");

	let settings = Settings::default();
	let git = repo.open();

	// Test Commit
	let commit_source = GitSource::Commit(c2.clone());
	let changed_commit = changed_items(
		&repo.canonical_id(),
		&git,
		&commit_source,
		&RunOptions::default(),
	)
	.unwrap();

	let expected_commit_paths = gitsrc::list_changed_paths_with(
		&git,
		&commit_source,
		&RunOptions::default(),
	)
	.unwrap();
	let actual_commit_paths: Vec<_> = changed_commit
		.items
		.iter()
		.map(|item| (item.relative_path.clone(), item.change_type))
		.collect();
	assert_eq!(actual_commit_paths, expected_commit_paths);

	let sel_commit = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_commit.items,
	)
	.unwrap();
	let plan_commit = plan_export(&sel_commit, &settings, None).unwrap();
	let legacy_commit = gitsrc::collect_payload(
		&git,
		&commit_source,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_commit.payload, legacy_commit.payload);

	// Test Merge Commit
	repo.git(&["checkout", "-b", "side", &c1]);
	repo.write("side.txt", "side content\n");
	let _ = repo.commit("side branch");

	repo.git(&["checkout", "main"]);
	repo.git(&["merge", "-m", "merge side", "side"]);
	let c_merge = repo.git(&["rev-parse", "HEAD"]);

	let merge_source = GitSource::Commit(c_merge);
	let changed_merge = changed_items(
		&repo.canonical_id(),
		&git,
		&merge_source,
		&RunOptions::default(),
	)
	.unwrap();
	let sel_merge = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_merge.items,
	)
	.unwrap();
	let plan_merge = plan_export(&sel_merge, &settings, None).unwrap();
	let legacy_merge =
		gitsrc::collect_payload(&git, &merge_source, &[repo.path()], &settings)
			.unwrap();
	assert_eq!(plan_merge.payload, legacy_merge.payload);

	// Test Staged
	repo.write("staged_file.txt", "staged\n");
	repo.git(&["add", "staged_file.txt"]);
	let staged_source = GitSource::Staged;
	let changed_staged = changed_items(
		&repo.canonical_id(),
		&git,
		&staged_source,
		&RunOptions::default(),
	)
	.unwrap();
	let expected_staged_paths = gitsrc::list_changed_paths_with(
		&git,
		&staged_source,
		&RunOptions::default(),
	)
	.unwrap();
	let actual_staged_paths: Vec<_> = changed_staged
		.items
		.iter()
		.map(|item| (item.relative_path.clone(), item.change_type))
		.collect();
	assert_eq!(actual_staged_paths, expected_staged_paths);

	let sel_staged = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_staged.items,
	)
	.unwrap();
	let plan_staged = plan_export(&sel_staged, &settings, None).unwrap();
	let legacy_staged = gitsrc::collect_payload(
		&git,
		&staged_source,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_staged.payload, legacy_staged.payload);
}

// ---------------------------------------------------------------------------
// 31. Phase 2: Range freshness and deleted content resolution
// ---------------------------------------------------------------------------

#[test]
fn test_range_freshness_revalidation_and_staleness() {
	let repo = TestRepo::new("range-freshness");
	repo.write("f1.txt", "a\n");
	repo.write("del.txt", "del base\n");
	let sha_a = repo.commit("A");

	repo.write("f1.txt", "b\n");
	repo.git(&["rm", "del.txt"]);
	let sha_b = repo.commit("B");

	let git = repo.open();
	let range_source = GitSource::Range(sha_a, sha_b);
	let settings = Settings::default();

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();
	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	assert!(plan.revalidate().is_ok());

	// Moving HEAD makes the plan stale
	repo.write("f2.txt", "c\n");
	repo.commit("C");
	assert!(matches!(
		plan.revalidate(),
		Err(TransferError::StaleSource { .. })
	));
}

#[test]
fn test_range_deletions_read_base_when_base_is_not_head() {
	let repo = TestRepo::new("range-del-base-not-head");
	repo.write("del.txt", "base content only in A\n");
	let sha_a = repo.commit("A");

	repo.git(&["rm", "del.txt"]);
	let sha_b = repo.commit("B");

	// Move HEAD to commit C, so HEAD != sha_a and HEAD != sha_b
	repo.write("other.txt", "c content\n");
	let _ = repo.commit("C");

	let git = repo.open();
	let range_source = GitSource::Range(sha_a, sha_b);
	let settings = Settings::default();

	let changed = changed_items(
		&repo.canonical_id(),
		&git,
		&range_source,
		&RunOptions::default(),
	)
	.unwrap();
	let sel = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed.items,
	)
	.unwrap();

	let plan = plan_export(&sel, &settings, None).unwrap();
	assert_eq!(plan.files.len(), 1);
	assert_eq!(
		plan.files[0].content.as_deref(),
		Some("base content only in A\n")
	);
}

// ---------------------------------------------------------------------------
// 32. Phase 2: changed_items gitlinks and non-UTF-8 skip count
// ---------------------------------------------------------------------------

#[test]
fn test_changed_items_gitlinks() {
	let repo = TestRepo::new("changed-items-gitlink");
	repo.write("a_before.txt", "before\n");
	repo.git(&["add", "a_before.txt"]);
	let fake_sha = "1234567890123456789012345678901234567890";
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		"160000",
		fake_sha,
		"submodule_link",
	]);
	repo.write("z_after.txt", "after\n");
	repo.git(&["add", "z_after.txt"]);

	let git = repo.open();
	let opts = RunOptions::default();
	let source = GitSource::Staged;
	let res =
		changed_items(&repo.canonical_id(), &git, &source, &opts).unwrap();

	assert_eq!(res.gitlinks, vec!["submodule_link"]);
	let gitlink_item = res
		.items
		.iter()
		.find(|item| item.relative_path == "submodule_link")
		.expect("gitlink must be in items");
	assert!(gitlink_item.gitlink);

	let (gitsrc_entries, _) =
		gitsrc::list_changes_with(&git, &source, &opts).unwrap();
	let gitsrc_paths: Vec<_> =
		gitsrc_entries.iter().map(|e| e.path.as_str()).collect();
	let changed_paths: Vec<_> =
		res.items.iter().map(|i| i.relative_path.as_str()).collect();
	assert_eq!(changed_paths, gitsrc_paths);
}

#[test]
#[cfg(unix)]
fn test_changed_items_non_utf8_skipped() {
	use std::io::Write;
	use std::os::unix::ffi::OsStrExt;

	let repo = TestRepo::new("changed-items-non-utf8");
	repo.write("valid.txt", "valid\n");

	let mut hash_cmd = Command::new("git");
	hash_cmd
		.args(["hash-object", "-w", "--stdin"])
		.current_dir(repo.path())
		.env("GIT_CONFIG_GLOBAL", &repo.cfg)
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped());
	let mut child = hash_cmd.spawn().unwrap();
	child
		.stdin
		.as_mut()
		.unwrap()
		.write_all(b"bad content\n")
		.unwrap();
	let out = child.wait_with_output().unwrap();
	let blob_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();

	let bad_name = std::ffi::OsStr::from_bytes(b"bad_\xff.txt");
	let mut update_cmd = Command::new("git");
	update_cmd
		.arg("update-index")
		.arg("--add")
		.arg("--cacheinfo")
		.arg("100644")
		.arg(&blob_sha)
		.arg(bad_name)
		.current_dir(repo.path())
		.env("GIT_CONFIG_GLOBAL", &repo.cfg);
	let update_out = update_cmd.output().unwrap();
	assert!(update_out.status.success());

	let git = repo.open();
	let res = changed_items(
		&repo.canonical_id(),
		&git,
		&GitSource::Working,
		&RunOptions::default(),
	)
	.unwrap();

	assert!(res.skipped_non_utf8 >= 1);
	assert!(res
		.items
		.iter()
		.any(|item| item.relative_path == "valid.txt"));
	assert!(res
		.items
		.iter()
		.all(|item| !item.relative_path.starts_with("bad_")));
}

// ---------------------------------------------------------------------------
// 33. Phase 2: selection_from_paths
// ---------------------------------------------------------------------------

#[test]
fn test_selection_from_paths_comprehensive() {
	let repo = TestRepo::new("sel-from-paths");
	repo.write("sub/file.txt", "hello sub\n");
	repo.write("root_file.txt", "hello root\n");

	// 1. Relative path via cwd
	let cwd = repo.path().join("sub");
	let sel_cwd =
		selection_from_paths(repo.path(), &cwd, &[PathBuf::from("file.txt")])
			.unwrap();
	assert_eq!(sel_cwd.sel.items.len(), 1);
	assert_eq!(sel_cwd.sel.items[0].relative_path, "sub/file.txt");
	assert_eq!(sel_cwd.skipped, 0);

	// 2. Absolute path
	let abs_path = repo.path().join("sub/file.txt");
	let sel_abs =
		selection_from_paths(repo.path(), repo.path(), &[abs_path]).unwrap();
	assert_eq!(sel_abs.sel.items.len(), 1);
	assert_eq!(sel_abs.sel.items[0].relative_path, "sub/file.txt");

	// 3. Missing -> PathNotFound
	let err_missing = selection_from_paths(
		repo.path(),
		repo.path(),
		&[PathBuf::from("missing.txt")],
	)
	.unwrap_err();
	assert!(matches!(err_missing, TransferError::PathNotFound(_)));

	// 4. Outside root -> PathOutsideRoot
	let err_outside_rel = selection_from_paths(
		repo.path(),
		repo.path(),
		&[PathBuf::from("../outside.txt")],
	)
	.unwrap_err();
	assert!(matches!(err_outside_rel, TransferError::PathOutsideRoot(_)));

	let other_dir = tempfile::tempdir().unwrap();
	let err_outside_abs = selection_from_paths(
		repo.path(),
		repo.path(),
		&[other_dir.path().join("other.txt")],
	)
	.unwrap_err();
	assert!(matches!(err_outside_abs, TransferError::PathOutsideRoot(_)));

	// 5. Dir stays a folder item
	let sel_dir =
		selection_from_paths(repo.path(), repo.path(), &[PathBuf::from("sub")])
			.unwrap();
	assert_eq!(sel_dir.sel.items.len(), 1);
	assert_eq!(sel_dir.sel.items[0].relative_path, "sub");

	// 6. Duplicates collapse
	let sel_dup = selection_from_paths(
		repo.path(),
		repo.path(),
		&[
			PathBuf::from("root_file.txt"),
			PathBuf::from("root_file.txt"),
		],
	)
	.unwrap();
	assert_eq!(sel_dup.sel.items.len(), 1);

	// 7. Empty result -> EmptySelection
	let err_empty =
		selection_from_paths(repo.path(), repo.path(), &[]).unwrap_err();
	assert!(matches!(err_empty, TransferError::EmptySelection));
}

#[test]
fn test_selection_from_paths_root_dot_and_nested_repo_pruning() {
	let repo = TestRepo::new("sel-root-dot");
	repo.write("f1.txt", "f1");
	repo.write("dir/f2.txt", "f2");

	// Nested repo
	let nested = repo.path().join("nested");
	fs::create_dir_all(&nested).unwrap();
	let nested_git = nested.join(".git");
	fs::create_dir_all(&nested_git).unwrap();
	fs::write(nested.join("nested_file.txt"), "nested").unwrap();

	// Test "."
	let res_dot =
		selection_from_paths(repo.path(), repo.path(), &[PathBuf::from(".")])
			.unwrap();

	let expanded_dot =
		expand_folder_items(res_dot.sel, 100, &CancelToken::new()).unwrap();
	let rels: Vec<_> = expanded_dot
		.sel
		.items
		.iter()
		.map(|item| item.relative_path.as_str())
		.collect();

	assert!(rels.contains(&"f1.txt"));
	assert!(rels.contains(&"dir/f2.txt"));
	assert!(rels.iter().all(|r| !r.starts_with(".git")));
	assert!(rels.iter().all(|r| !r.starts_with("nested")));

	// Test root path itself
	let res_root = selection_from_paths(
		repo.path(),
		repo.path(),
		&[repo.path().to_path_buf()],
	)
	.unwrap();

	let expanded_root =
		expand_folder_items(res_root.sel, 100, &CancelToken::new()).unwrap();
	let rels_root: Vec<_> = expanded_root
		.sel
		.items
		.iter()
		.map(|item| item.relative_path.as_str())
		.collect();

	assert!(rels_root.contains(&"f1.txt"));
	assert!(rels_root.contains(&"dir/f2.txt"));
	assert!(rels_root.iter().all(|r| !r.starts_with(".git")));
	assert!(rels_root.iter().all(|r| !r.starts_with("nested")));
}

#[test]
#[cfg(unix)]
fn test_selection_from_paths_symlinks_and_fifo() {
	let repo = TestRepo::new("sel-symlinks");
	repo.write("target.txt", "target\n");

	// In-root symlink to in-root file
	std::os::unix::fs::symlink(
		repo.path().join("target.txt"),
		repo.path().join("in_root_link.txt"),
	)
	.unwrap();

	// Out-of-root symlink
	let outside = tempfile::tempdir().unwrap();
	let outside_file = outside.path().join("outside.txt");
	fs::write(&outside_file, "outside").unwrap();
	std::os::unix::fs::symlink(
		&outside_file,
		repo.path().join("outside_link.txt"),
	)
	.unwrap();

	let res = selection_from_paths(
		repo.path(),
		repo.path(),
		&[
			PathBuf::from("in_root_link.txt"),
			PathBuf::from("outside_link.txt"),
		],
	)
	.unwrap();

	assert_eq!(res.sel.items.len(), 1);
	assert_eq!(res.sel.items[0].relative_path, "in_root_link.txt");
	assert_eq!(res.skipped, 1);

	// FIFO skipped at root level with timeout pattern
	let fifo_path = repo.path().join("fifo_pipe");
	let status = Command::new("mkfifo").arg(&fifo_path).status();
	match status {
		Ok(s) if s.success() => {}
		_ => {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"mkfifo command failed or unavailable"
			);
			return;
		}
	}

	let (tx, rx) = std::sync::mpsc::channel();
	let repo_path = repo.path().to_path_buf();
	std::thread::spawn(move || {
		let res =
			selection_from_paths(&repo_path, &repo_path, &[PathBuf::from(".")]);
		let _ = tx.send(res);
	});

	let path_sel = rx
		.recv_timeout(std::time::Duration::from_secs(5))
		.expect("selection_from_paths timed out on FIFO")
		.expect("selection_from_paths failed");

	assert!(path_sel
		.sel
		.items
		.iter()
		.all(|item| item.relative_path != "fifo_pipe"));
	assert!(path_sel.skipped >= 1);
}

#[test]
#[cfg(unix)]
fn test_selection_from_paths_root_through_symlink() {
	let repo = TestRepo::new("sel-symlink-root");
	repo.write("file.txt", "content\n");

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	let res = selection_from_paths(
		&alias_path,
		&alias_path,
		&[PathBuf::from("file.txt")],
	)
	.unwrap();

	assert_eq!(res.sel.roots.len(), 1);
	assert_eq!(res.sel.roots[0], repo.canonical_id());
	assert_eq!(res.sel.primary_root, Some(repo.canonical_id()));
	assert_eq!(res.sel.items.len(), 1);
	assert_eq!(res.sel.items[0].relative_path, "file.txt");

	let sub = repo.path().join("sub");
	fs::create_dir(&sub).unwrap();
	let alias_sub = alias_path.join("sub");

	let expected =
		selection_from_paths(repo.path(), repo.path(), &[PathBuf::from(".")])
			.unwrap();

	// 1. "." with cwd = alias
	let case_dot =
		selection_from_paths(&alias_path, &alias_path, &[PathBuf::from(".")])
			.unwrap();
	assert_eq!(case_dot.sel.items, expected.sel.items);

	// 2. The alias path itself as the argument (cwd elsewhere)
	let elsewhere = alias_dir.path();
	let case_alias = selection_from_paths(
		&alias_path,
		elsewhere,
		std::slice::from_ref(&alias_path),
	)
	.unwrap();
	assert_eq!(case_alias.sel.items, expected.sel.items);

	// 3. ".." with cwd alias/sub
	let case_dotdot =
		selection_from_paths(&alias_path, &alias_sub, &[PathBuf::from("..")])
			.unwrap();
	assert_eq!(case_dotdot.sel.items, expected.sel.items);
}

#[cfg(unix)]
#[test]
fn test_selection_from_paths_labels_and_explicit_dir_symlink() {
	let repo = TestRepo::new("sel-labels-dir-symlink");
	repo.write("sub/s.txt", "content s\n");
	std::os::unix::fs::symlink("sub", repo.path().join("linkdir")).unwrap();

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	// 1. Label of file reached through directory symlink with symlink-spelled root:
	let res_file = selection_from_paths(
		&alias_path,
		&alias_path,
		&[PathBuf::from("linkdir/s.txt")],
	)
	.unwrap();
	assert_eq!(res_file.sel.source_root, Some("repo_alias".to_string()));
	assert_eq!(res_file.sel.items.len(), 1);
	assert_eq!(res_file.sel.items[0].relative_path, "linkdir/s.txt");

	let cancel = CancelToken::new();
	let expanded_file =
		expand_folder_items(res_file.sel, 100, &cancel).unwrap();
	assert_eq!(
		expanded_file.sel.source_root,
		Some("repo_alias".to_string())
	);
	let plan_file =
		plan_export(&expanded_file.sel, &Settings::default(), None).unwrap();
	assert!(plan_file.payload.contains("// clipcode-root: repo_alias"));
	assert!(plan_file.payload.contains("// file: linkdir/s.txt"));

	// 2. Explicitly passed directory symlink `linkdir`:
	let res_dir = selection_from_paths(
		&alias_path,
		&alias_path,
		&[PathBuf::from("linkdir")],
	)
	.unwrap();
	assert_eq!(res_dir.sel.source_root, Some("repo_alias".to_string()));
	assert_eq!(res_dir.sel.items.len(), 1);
	assert_eq!(res_dir.sel.items[0].relative_path, "linkdir");

	let expanded_dir = expand_folder_items(res_dir.sel, 100, &cancel).unwrap();
	assert_eq!(expanded_dir.sel.source_root, Some("repo_alias".to_string()));
	let dir_rels: Vec<_> = expanded_dir
		.sel
		.items
		.iter()
		.map(|it| it.relative_path.as_str())
		.collect();
	assert_eq!(dir_rels, vec!["linkdir/s.txt"]);

	let plan_dir =
		plan_export(&expanded_dir.sel, &Settings::default(), None).unwrap();
	assert!(plan_dir.payload.contains("// clipcode-root: repo_alias"));
	assert!(plan_dir.payload.contains("// file: linkdir/s.txt"));
}

// ---------------------------------------------------------------------------
// 34. Phase 2: from_restore_base equivalence
// ---------------------------------------------------------------------------

#[test]
fn test_from_restore_base_equivalence() {
	let repo = TestRepo::new("from-restore-base");
	let primary = repo.canonical_id();

	// 1. Strip
	let strip_suggestion = RestoreBaseSuggestion {
		base: RestoreBase::Strip {
			segment: "pkg".to_string(),
		},
		label: "strip pkg/".to_string(),
		matched: 2,
		total: 2,
	};
	let strip_mapping =
		ImportMapping::from_restore_base(&strip_suggestion, primary.clone());

	let entries_strip = [
		("pkg/src/lib.rs", "fn lib() {}"),
		("pkg/main.rs", "fn main() {}"),
		("other/doc.md", "# Doc"),
		("README.md", "# Readme"),
	];
	let mut payload_strip = String::new();
	for (p, c) in entries_strip {
		payload_strip.push_str(&format!("file: {p}\n{c}\n"));
	}

	let plan_strip = plan_import_with(
		&payload_strip,
		"file: $FILE_PATH",
		&[repo.path().to_path_buf()],
		&strip_mapping,
		&RunOptions::default(),
	)
	.unwrap();

	for (p, c) in entries_strip {
		let expected_rel =
			restore::apply_restore_base(&strip_suggestion.base, p);
		let parsed = format::ParsedEntry {
			path: expected_rel.clone(),
			content: c.to_string(),
			change_types: std::collections::BTreeSet::from([ChangeType::New]),
		};
		let expected_plan = restore::plan_restore(&[repo.path()], &[parsed]);
		let actual_op = plan_strip
			.create_operations()
			.iter()
			.find(|op| op.relative_path == expected_rel)
			.unwrap_or_else(|| panic!("op found for {expected_rel}"));
		assert_eq!(
			actual_op.relative_path,
			expected_plan.create_operations[0].relative_path
		);
	}

	// 2. Add
	let add_suggestion = RestoreBaseSuggestion {
		base: RestoreBase::Add {
			prefix: "nested".to_string(),
		},
		label: "add nested/".to_string(),
		matched: 2,
		total: 2,
	};
	let add_mapping =
		ImportMapping::from_restore_base(&add_suggestion, primary);

	let entries_add = [
		("src/lib.rs", "fn lib() {}"),
		("README.md", "# Readme"),
		("/elsewhere/x.txt", "fn x() {}"),
	];
	let mut payload_add = String::new();
	for (p, c) in entries_add {
		payload_add.push_str(&format!("file: {p}\n{c}\n"));
	}

	let plan_add = plan_import_with(
		&payload_add,
		"file: $FILE_PATH",
		&[repo.path().to_path_buf()],
		&add_mapping,
		&RunOptions::default(),
	)
	.unwrap();

	for (p, c) in entries_add {
		let expected_path = if restore::is_relative(p) {
			restore::apply_restore_base(&add_suggestion.base, p)
		} else {
			p.to_string()
		};
		let parsed = format::ParsedEntry {
			path: expected_path,
			content: c.to_string(),
			change_types: std::collections::BTreeSet::from([ChangeType::New]),
		};
		let expected_plan = restore::plan_restore(&[repo.path()], &[parsed]);
		let expected_rel = &expected_plan.create_operations[0].relative_path;
		let actual_op = plan_add
			.create_operations()
			.iter()
			.find(|op| &op.relative_path == expected_rel)
			.unwrap_or_else(|| panic!("op found for {expected_rel}"));
		assert_eq!(
			actual_op.relative_path,
			expected_plan.create_operations[0].relative_path
		);
		if p == "/elsewhere/x.txt" {
			assert_eq!(actual_op.relative_path, "elsewhere/x.txt");
		}
	}
}

#[test]
#[cfg(unix)]
fn test_from_restore_base_root_through_symlink() {
	let repo = TestRepo::new("restore-base-symlink");
	let alias_dir = tempfile::tempdir().unwrap();
	let alias_path = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_path).unwrap();

	let canonical_primary = CanonicalRootId::new(&alias_path).unwrap();
	let suggestion = RestoreBaseSuggestion {
		base: RestoreBase::Strip {
			segment: "pkg".to_string(),
		},
		label: "strip pkg/".to_string(),
		matched: 1,
		total: 1,
	};
	let mapping =
		ImportMapping::from_restore_base(&suggestion, canonical_primary);

	let payload = "file: pkg/a.txt\nhello\n";
	let plan = plan_import_with(
		payload,
		"file: $FILE_PATH",
		&[alias_path],
		&mapping,
		&RunOptions::default(),
	)
	.unwrap();

	assert_eq!(plan.create_operations().len(), 1);
	assert_eq!(plan.create_operations()[0].relative_path, "a.txt");
	assert_eq!(
		plan.create_operations()[0].root_path,
		repo.canonical_id().path()
	);
}

// ---------------------------------------------------------------------------
// 35. Phase 2: CLIPBOARD_PAYLOAD_MAX constant is 32 MiB
// ---------------------------------------------------------------------------

#[test]
fn test_clipboard_payload_max_is_32_mib() {
	assert_eq!(CLIPBOARD_PAYLOAD_MAX, 32 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// 36. Phase 2: changed_items and plan_export with subdir roots
// ---------------------------------------------------------------------------

#[test]
fn test_changed_items_and_plan_export_subdir_root_with_decoy() {
	let repo = TestRepo::new("subdir-decoy");
	repo.write("sub/a.txt", "real initial\n");
	repo.write("sub/sub/a.txt", "DECOY\n");
	repo.write("sub/del.txt", "deleted content\n");
	repo.write("outside.txt", "outside initial\n");
	let base_sha = repo.commit("base commit");

	let sub_path = repo.path().join("sub");
	let sub_root = CanonicalRootId::new(&sub_path).unwrap();
	let opts = RunOptions::default();
	let settings = Settings::default();

	// 1. Working: modify sub/a.txt, rm sub/del.txt, modify outside.txt
	repo.write("sub/a.txt", "real modified working\n");
	repo.git(&["rm", "sub/del.txt"]);
	repo.write("outside.txt", "outside modified working\n");

	let git = repo.open();
	let changed_working =
		changed_items(&sub_root, &git, &GitSource::Working, &opts).unwrap();
	assert!(changed_working
		.out_of_scope
		.contains(&"outside.txt".to_string()));
	let working_item_paths: Vec<_> = changed_working
		.items
		.iter()
		.map(|i| i.relative_path.as_str())
		.collect();
	assert_eq!(working_item_paths, vec!["a.txt", "del.txt"]);

	let sel_working = ExportSelection::new(
		vec![sub_path.clone()],
		Some(sub_path.clone()),
		changed_working.items,
	)
	.unwrap();
	let plan_working = plan_export(&sel_working, &settings, None).unwrap();
	assert!(plan_working.payload.contains("real modified working\n"));
	assert!(!plan_working.payload.contains("DECOY"));
	assert!(!plan_working.payload.contains("outside"));
	assert!(plan_working.payload.contains("deleted content\n"));
	assert!(plan_working.files.iter().any(|f| f.path == "a.txt"));
	assert!(plan_working.files.iter().any(|f| f.path == "del.txt"));
	assert!(!plan_working
		.files
		.iter()
		.any(|f| f.path.contains("outside")));

	// 2. Staged
	repo.git(&["add", "sub/a.txt", "outside.txt"]);
	let changed_staged =
		changed_items(&sub_root, &git, &GitSource::Staged, &opts).unwrap();
	assert!(changed_staged
		.out_of_scope
		.contains(&"outside.txt".to_string()));
	let staged_item_paths: Vec<_> = changed_staged
		.items
		.iter()
		.map(|i| i.relative_path.as_str())
		.collect();
	assert_eq!(staged_item_paths, vec!["a.txt", "del.txt"]);

	let sel_staged = ExportSelection::new(
		vec![sub_path.clone()],
		Some(sub_path.clone()),
		changed_staged.items,
	)
	.unwrap();
	let plan_staged = plan_export(&sel_staged, &settings, None).unwrap();
	assert!(plan_staged.payload.contains("real modified working\n"));
	assert!(!plan_staged.payload.contains("DECOY"));
	assert!(!plan_staged.payload.contains("outside"));
	assert!(plan_staged.payload.contains("deleted content\n"));
	assert!(plan_staged.files.iter().any(|f| f.path == "a.txt"));
	assert!(plan_staged.files.iter().any(|f| f.path == "del.txt"));

	// 3. Commit
	let commit_sha = repo.commit("commit with sub changes");
	let changed_commit = changed_items(
		&sub_root,
		&git,
		&GitSource::Commit(commit_sha.clone()),
		&opts,
	)
	.unwrap();
	assert!(changed_commit
		.out_of_scope
		.contains(&"outside.txt".to_string()));
	let commit_item_paths: Vec<_> = changed_commit
		.items
		.iter()
		.map(|i| i.relative_path.as_str())
		.collect();
	assert_eq!(commit_item_paths, vec!["a.txt", "del.txt"]);

	let sel_commit = ExportSelection::new(
		vec![sub_path.clone()],
		Some(sub_path.clone()),
		changed_commit.items,
	)
	.unwrap();
	let plan_commit = plan_export(&sel_commit, &settings, None).unwrap();
	assert!(plan_commit.payload.contains("real modified working\n"));
	assert!(!plan_commit.payload.contains("DECOY"));
	assert!(!plan_commit.payload.contains("outside"));
	assert!(plan_commit.payload.contains("deleted content\n"));
	assert!(plan_commit.files.iter().any(|f| f.path == "a.txt"));
	assert!(plan_commit.files.iter().any(|f| f.path == "del.txt"));

	// 4. Range
	let range_source = GitSource::Range(base_sha, commit_sha);
	let changed_range =
		changed_items(&sub_root, &git, &range_source, &opts).unwrap();
	assert!(changed_range
		.out_of_scope
		.contains(&"outside.txt".to_string()));
	let range_item_paths: Vec<_> = changed_range
		.items
		.iter()
		.map(|i| i.relative_path.as_str())
		.collect();
	assert_eq!(range_item_paths, vec!["a.txt", "del.txt"]);

	let sel_range = ExportSelection::new(
		vec![sub_path.clone()],
		Some(sub_path),
		changed_range.items,
	)
	.unwrap();
	let plan_range = plan_export(&sel_range, &settings, None).unwrap();
	assert!(plan_range.payload.contains("real modified working\n"));
	assert!(!plan_range.payload.contains("DECOY"));
	assert!(!plan_range.payload.contains("outside"));
	assert!(plan_range.payload.contains("deleted content\n"));
	assert!(plan_range.files.iter().any(|f| f.path == "a.txt"));
	assert!(plan_range.files.iter().any(|f| f.path == "del.txt"));
}

#[test]
#[cfg(unix)]
fn test_changed_items_and_plan_export_subdir_symlink() {
	let repo = TestRepo::new("subdir-symlink");
	repo.write("sub/a.txt", "real file\n");
	repo.write("sub/sub/a.txt", "DECOY\n");
	repo.write("outside.txt", "outside file\n");
	repo.commit("base");

	repo.write("sub/a.txt", "real modified\n");
	repo.write("outside.txt", "outside modified\n");

	let alias_dir = tempfile::tempdir().unwrap();
	let alias_repo = alias_dir.path().join("repo_alias");
	std::os::unix::fs::symlink(repo.path(), &alias_repo).unwrap();

	let alias_sub = alias_repo.join("sub");
	let sub_root = CanonicalRootId::new(&alias_sub).unwrap();
	let git = Git::open(&alias_sub).unwrap();
	let opts = RunOptions::default();

	let changed =
		changed_items(&sub_root, &git, &GitSource::Working, &opts).unwrap();
	assert_eq!(changed.items.len(), 1);
	assert_eq!(changed.items[0].relative_path, "a.txt");
	assert!(changed.out_of_scope.contains(&"outside.txt".to_string()));

	let sel = ExportSelection::new(
		vec![alias_sub.clone()],
		Some(alias_sub),
		changed.items,
	)
	.unwrap();
	let plan = plan_export(&sel, &Settings::default(), None).unwrap();
	assert!(plan.payload.contains("real modified\n"));
	assert!(!plan.payload.contains("DECOY"));
	assert!(!plan.payload.contains("outside"));
}

#[test]
fn test_changed_items_root_outside_toplevel_errors() {
	let repo = TestRepo::new("root-outside");
	let outside_dir = tempfile::tempdir().unwrap();
	let outside_root = CanonicalRootId::new(outside_dir.path()).unwrap();
	let git = repo.open();
	let err = changed_items(
		&outside_root,
		&git,
		&GitSource::Working,
		&RunOptions::default(),
	)
	.unwrap_err();
	match err {
		TransferError::UnknownRoot(p) => {
			assert_eq!(p, outside_root.path());
		}
		other => panic!("expected TransferError::UnknownRoot, got {other:?}"),
	}
}

// ---------------------------------------------------------------------------
// 37. Phase 2: Gitlink parity with legacy gitsrc (Staged, Commit, Range)
// ---------------------------------------------------------------------------

#[test]
fn test_parity_staged_gitlink_exists_and_missing() {
	let repo = TestRepo::new("staged-gitlink-parity");
	repo.write("a.txt", "file a\n");
	let base = repo.commit("base");

	let settings = Settings::default();
	let git = repo.open();
	let opts = RunOptions::default();

	// (i) Gitlink whose commit exists in the odb
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},sub_exists"),
	]);

	let changed_exists =
		changed_items(&repo.canonical_id(), &git, &GitSource::Staged, &opts)
			.unwrap();
	assert!(changed_exists.gitlinks.contains(&"sub_exists".to_string()));

	let sel_exists = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_exists.items,
	)
	.unwrap();
	let plan_exists = plan_export(&sel_exists, &settings, None).unwrap();
	let legacy_exists = gitsrc::collect_payload(
		&git,
		&GitSource::Staged,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_exists.payload, legacy_exists.payload);
	assert_eq!(
		plan_exists.copied_file_count,
		legacy_exists.copied_file_count
	);
	assert_eq!(
		plan_exists.skipped_unreadable_count,
		legacy_exists.skipped_unreadable_count
	);
	assert_eq!(
		plan_exists.skipped_file_size_count,
		legacy_exists.skipped_file_size_count
	);

	// (ii) Gitlink whose sha is NOT in the odb
	let fake_sha = "1234567890123456789012345678901234567890";
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{fake_sha},sub_missing"),
	]);

	let changed_missing =
		changed_items(&repo.canonical_id(), &git, &GitSource::Staged, &opts)
			.unwrap();
	let sel_missing = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_missing.items,
	)
	.unwrap();
	let plan_missing = plan_export(&sel_missing, &settings, None).unwrap();
	let legacy_missing = gitsrc::collect_payload(
		&git,
		&GitSource::Staged,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_missing.payload, legacy_missing.payload);
	assert_eq!(
		plan_missing.copied_file_count,
		legacy_missing.copied_file_count
	);
	assert_eq!(
		plan_missing.skipped_unreadable_count,
		legacy_missing.skipped_unreadable_count
	);
	assert_eq!(
		plan_missing.skipped_file_size_count,
		legacy_missing.skipped_file_size_count
	);
}

#[test]
fn test_parity_commit_gitlink_exists_and_missing() {
	let repo = TestRepo::new("commit-gitlink-parity");
	repo.write("a.txt", "file a\n");
	let base = repo.commit("base");

	let settings = Settings::default();
	let git = repo.open();
	let opts = RunOptions::default();

	// (i) Gitlink whose commit exists in the odb
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},sub_exists"),
	]);
	repo.git(&["commit", "-m", "add existing gitlink"]);
	let commit_exists = repo.git(&["rev-parse", "HEAD"]);

	let source_exists = GitSource::Commit(commit_exists);
	let changed_exists =
		changed_items(&repo.canonical_id(), &git, &source_exists, &opts)
			.unwrap();
	assert!(changed_exists.gitlinks.contains(&"sub_exists".to_string()));

	let sel_exists = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_exists.items,
	)
	.unwrap();
	let plan_exists = plan_export(&sel_exists, &settings, None).unwrap();
	let legacy_exists = gitsrc::collect_payload(
		&git,
		&source_exists,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_exists.payload, legacy_exists.payload);
	assert_eq!(
		plan_exists.copied_file_count,
		legacy_exists.copied_file_count
	);
	assert_eq!(
		plan_exists.skipped_unreadable_count,
		legacy_exists.skipped_unreadable_count
	);
	assert_eq!(
		plan_exists.skipped_file_size_count,
		legacy_exists.skipped_file_size_count
	);

	// (ii) Gitlink whose sha is NOT in the odb
	let fake_sha = "1234567890123456789012345678901234567890";
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{fake_sha},sub_missing"),
	]);
	repo.git(&["commit", "-m", "add missing gitlink"]);
	let commit_missing = repo.git(&["rev-parse", "HEAD"]);

	let source_missing = GitSource::Commit(commit_missing);
	let changed_missing =
		changed_items(&repo.canonical_id(), &git, &source_missing, &opts)
			.unwrap();
	let sel_missing = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_missing.items,
	)
	.unwrap();
	let plan_missing = plan_export(&sel_missing, &settings, None).unwrap();
	let legacy_missing = gitsrc::collect_payload(
		&git,
		&source_missing,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_missing.payload, legacy_missing.payload);
	assert_eq!(
		plan_missing.copied_file_count,
		legacy_missing.copied_file_count
	);
	assert_eq!(
		plan_missing.skipped_unreadable_count,
		legacy_missing.skipped_unreadable_count
	);
	assert_eq!(
		plan_missing.skipped_file_size_count,
		legacy_missing.skipped_file_size_count
	);
}

#[test]
fn test_parity_range_gitlink_exists_and_missing() {
	let repo = TestRepo::new("range-gitlink-parity");
	repo.write("a.txt", "file a\n");
	let base = repo.commit("base");

	let settings = Settings::default();
	let git = repo.open();
	let opts = RunOptions::default();

	// (i) Gitlink whose commit exists in the odb
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},sub_exists"),
	]);
	repo.git(&["commit", "-m", "range tip with existing gitlink"]);
	let tip_exists = repo.git(&["rev-parse", "HEAD"]);

	let source_exists = GitSource::Range(base.clone(), tip_exists);
	let changed_exists =
		changed_items(&repo.canonical_id(), &git, &source_exists, &opts)
			.unwrap();
	assert!(changed_exists.gitlinks.contains(&"sub_exists".to_string()));

	let sel_exists = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_exists.items,
	)
	.unwrap();
	let plan_exists = plan_export(&sel_exists, &settings, None).unwrap();
	let legacy_exists = gitsrc::collect_payload(
		&git,
		&source_exists,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_exists.payload, legacy_exists.payload);
	assert_eq!(
		plan_exists.copied_file_count,
		legacy_exists.copied_file_count
	);
	assert_eq!(
		plan_exists.skipped_unreadable_count,
		legacy_exists.skipped_unreadable_count
	);
	assert_eq!(
		plan_exists.skipped_file_size_count,
		legacy_exists.skipped_file_size_count
	);

	// (ii) Gitlink whose sha is NOT in the odb
	let fake_sha = "1234567890123456789012345678901234567890";
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{fake_sha},sub_missing"),
	]);
	repo.git(&["commit", "-m", "range tip with missing gitlink"]);
	let tip_missing = repo.git(&["rev-parse", "HEAD"]);

	let source_missing = GitSource::Range(base, tip_missing);
	let changed_missing =
		changed_items(&repo.canonical_id(), &git, &source_missing, &opts)
			.unwrap();
	let sel_missing = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_missing.items,
	)
	.unwrap();
	let plan_missing = plan_export(&sel_missing, &settings, None).unwrap();
	let legacy_missing = gitsrc::collect_payload(
		&git,
		&source_missing,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_missing.payload, legacy_missing.payload);
	assert_eq!(
		plan_missing.copied_file_count,
		legacy_missing.copied_file_count
	);
	assert_eq!(
		plan_missing.skipped_unreadable_count,
		legacy_missing.skipped_unreadable_count
	);
	assert_eq!(
		plan_missing.skipped_file_size_count,
		legacy_missing.skipped_file_size_count
	);
}

#[test]
fn test_parity_working_gitlink_absent_and_present_dir_and_subdir() {
	let repo = TestRepo::new("working-gitlink-parity");
	repo.write("a.txt", "file a\n");
	repo.write("sub/inner.txt", "inner a\n");
	let base = repo.commit("base");

	repo.write("a.txt", "file a modified\n");
	repo.write("sub/inner.txt", "inner a modified\n");

	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},gitlink_top"),
	]);
	repo.git(&[
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{base},sub/gitlink_sub"),
	]);

	// (i) Working parity when gitlink directories are absent on disk:
	// --assume-unchanged keeps them as staged additions without reporting them as
	// deleted worktree paths while the directory is absent from disk.
	repo.git(&[
		"update-index",
		"--assume-unchanged",
		"gitlink_top",
		"sub/gitlink_sub",
	]);
	assert!(!repo.path().join("gitlink_top").exists());
	assert!(!repo.path().join("sub/gitlink_sub").exists());

	let settings = Settings::default();
	let opts = RunOptions::default();
	let git = repo.open();

	let changed_absent =
		changed_items(&repo.canonical_id(), &git, &GitSource::Working, &opts)
			.unwrap();
	assert!(changed_absent.gitlinks.contains(&"gitlink_top".to_string()));
	assert!(changed_absent
		.gitlinks
		.contains(&"sub/gitlink_sub".to_string()));

	let sel_absent = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_absent.items,
	)
	.unwrap();
	let plan_absent = plan_export(&sel_absent, &settings, None).unwrap();
	let legacy_absent = gitsrc::collect_payload(
		&git,
		&GitSource::Working,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_absent.payload, legacy_absent.payload);
	assert_eq!(
		plan_absent.copied_file_count,
		legacy_absent.copied_file_count
	);
	assert_eq!(
		plan_absent.skipped_unreadable_count,
		legacy_absent.skipped_unreadable_count
	);
	assert_eq!(
		plan_absent.skipped_file_size_count,
		legacy_absent.skipped_file_size_count
	);
	assert_eq!(plan_absent.copied_file_count, 2);
	assert_eq!(plan_absent.skipped_unreadable_count, 0);

	// (ii) Working parity when empty gitlink directories exist on disk
	repo.git(&[
		"update-index",
		"--no-assume-unchanged",
		"gitlink_top",
		"sub/gitlink_sub",
	]);
	std::fs::create_dir(repo.path().join("gitlink_top")).unwrap();
	std::fs::create_dir(repo.path().join("sub/gitlink_sub")).unwrap();

	let changed_present =
		changed_items(&repo.canonical_id(), &git, &GitSource::Working, &opts)
			.unwrap();
	let sel_present = ExportSelection::new(
		vec![repo.path().to_path_buf()],
		Some(repo.path().to_path_buf()),
		changed_present.items,
	)
	.unwrap();
	let plan_present = plan_export(&sel_present, &settings, None).unwrap();
	let legacy_present = gitsrc::collect_payload(
		&git,
		&GitSource::Working,
		&[repo.path()],
		&settings,
	)
	.unwrap();
	assert_eq!(plan_present.payload, legacy_present.payload);
	assert_eq!(
		plan_present.copied_file_count,
		legacy_present.copied_file_count
	);
	assert_eq!(
		plan_present.skipped_unreadable_count,
		legacy_present.skipped_unreadable_count
	);
	assert_eq!(
		plan_present.skipped_file_size_count,
		legacy_present.skipped_file_size_count
	);
	assert_eq!(plan_present.copied_file_count, 2);
	assert_eq!(plan_present.skipped_unreadable_count, 0);

	// (iii) Subdirectory root variant
	let sub_dir = repo.path().join("sub");
	let sub_id = CanonicalRootId::new(&sub_dir).unwrap();
	let changed_sub =
		changed_items(&sub_id, &git, &GitSource::Working, &opts).unwrap();
	assert!(changed_sub.out_of_scope.contains(&"a.txt".to_string()));
	assert!(changed_sub
		.out_of_scope
		.contains(&"gitlink_top".to_string()));
	assert!(changed_sub.gitlinks.contains(&"gitlink_sub".to_string()));

	let sel_sub = ExportSelection::new(
		vec![sub_dir.clone()],
		Some(sub_dir.clone()),
		changed_sub.items.clone(),
	)
	.unwrap();
	let plan_sub = plan_export(&sel_sub, &settings, None).unwrap();
	let legacy_sub = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Working,
		&[&sub_dir],
		&settings,
		Some(&sub_dir),
		None,
	)
	.unwrap();
	assert_eq!(plan_sub.payload, legacy_sub.payload);
	assert_eq!(plan_sub.copied_file_count, legacy_sub.copied_file_count);
	assert_eq!(
		plan_sub.skipped_unreadable_count,
		legacy_sub.skipped_unreadable_count
	);
	assert_eq!(
		plan_sub.skipped_file_size_count,
		legacy_sub.skipped_file_size_count
	);
	assert_eq!(plan_sub.copied_file_count, 1);
	assert_eq!(plan_sub.files[0].path, "inner.txt");

	// (iv) File count limit parity: gitlinks do not consume slots
	let limit_settings = Settings {
		set_max_file_count: true,
		file_count_limit: 1.0,
		..Settings::default()
	};
	let plan_limit = plan_export(&sel_sub, &limit_settings, None).unwrap();
	let legacy_limit = gitsrc::collect_payload_with_selection(
		&git,
		&GitSource::Working,
		&[&sub_dir],
		&limit_settings,
		Some(&sub_dir),
		None,
	)
	.unwrap();
	assert_eq!(plan_limit.payload, legacy_limit.payload);
	assert_eq!(plan_limit.copied_file_count, legacy_limit.copied_file_count);
	assert_eq!(
		plan_limit.file_limit_reached,
		legacy_limit.file_limit_reached
	);

	// Explicitly verify gitlink does not consume a slot when placed before a copyable file
	let mut reordered_items = changed_sub.items.clone();
	reordered_items.sort_by_key(|item| !item.gitlink);
	assert!(reordered_items[0].gitlink);
	let sel_reordered =
		ExportSelection::new(vec![sub_dir], None, reordered_items).unwrap();
	let plan_reordered =
		plan_export(&sel_reordered, &limit_settings, None).unwrap();
	assert_eq!(plan_reordered.copied_file_count, 1);
	assert_eq!(plan_reordered.files[0].path, "inner.txt");
	assert!(!plan_reordered.file_limit_reached);

	// (v) Unstaged source kind also skips gitlinks without touching disk
	let unstaged_items: Vec<_> = changed_sub
		.items
		.into_iter()
		.map(|mut it| {
			it.source = SourceKind::Unstaged;
			it
		})
		.collect();
	let sel_unstaged = ExportSelection::new(
		vec![repo.path().join("sub")],
		None,
		unstaged_items,
	)
	.unwrap();
	let plan_unstaged = plan_export(&sel_unstaged, &settings, None).unwrap();
	assert_eq!(plan_unstaged.copied_file_count, 1);
	assert_eq!(plan_unstaged.files[0].path, "inner.txt");
}

#[test]
fn test_plan_commit_export_with() {
	let repo = TestRepo::new("commit-export-with");
	repo.write("a.txt", "alpha\n");
	let c1 = repo.commit("commit 1");
	repo.write("b.txt", "beta\n");
	let _c2 = repo.commit("commit 2");
	repo.write("c.txt", "gamma\n");
	let c3 = repo.commit("commit 3");
	let git = repo.open();

	// (a) under cap, export.text is byte-identical to commits::to_clipboard_text(&plan_commit_export(...).unwrap())
	// for both range and last forms
	let legacy_range =
		plan_commit_export(&git, Some((&c1, &c3)), None).unwrap();
	let expected_range_text =
		snip_core::commits::to_clipboard_text(&legacy_range);
	let export_range = plan_commit_export_with(
		&git,
		Some((&c1, &c3)),
		None,
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap();
	assert_eq!(export_range.text, expected_range_text);
	assert_eq!(export_range.payload, legacy_range);

	let legacy_last = plan_commit_export(&git, None, Some(2)).unwrap();
	let expected_last_text =
		snip_core::commits::to_clipboard_text(&legacy_last);
	let export_last = plan_commit_export_with(
		&git,
		None,
		Some(2),
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap();
	assert_eq!(export_last.text, expected_last_text);
	assert_eq!(export_last.payload, legacy_last);

	// (b) discontinuous range -> TransferError::DiscontinuousCommits
	repo.git(&["checkout", "-b", "side"]);
	repo.write("side.txt", "side\n");
	let side_tip = repo.commit("side commit");
	repo.git(&["checkout", "main"]);
	repo.write("d.txt", "delta\n");
	let _ = repo.commit("commit 4");
	repo.git(&["merge", "side", "-m", "merge side"]);
	let main_tip = repo.git(&["rev-parse", "HEAD"]);

	let err_discontinuous = plan_commit_export_with(
		&git,
		Some((&side_tip, &main_tip)),
		None,
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap_err();
	assert!(
		matches!(
			err_discontinuous,
			TransferError::DiscontinuousCommits {
				ref base,
				ref tip,
				..
			} if base == &side_tip && tip == &main_tip
		),
		"expected DiscontinuousCommits, got: {err_discontinuous:?}"
	);

	// (c) tiny cap (e.g. 64) -> TransferError::Commit(CommitError::PayloadLimit{..})
	let err_capped = plan_commit_export_with(
		&git,
		None,
		Some(1),
		&RunOptions::default(),
		64,
	)
	.unwrap_err();
	assert!(
		matches!(
			err_capped,
			TransferError::Commit(
				snip_core::commits::CommitError::PayloadLimit { limit: 64, .. }
			)
		),
		"expected PayloadLimit with limit 64, got: {err_capped:?}"
	);

	// (d) neither/both of range and last -> EmptySelection
	let err_neither = plan_commit_export_with(
		&git,
		None,
		None,
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap_err();
	assert!(
		matches!(err_neither, TransferError::EmptySelection),
		"expected EmptySelection, got: {err_neither:?}"
	);

	let err_both = plan_commit_export_with(
		&git,
		Some((&c1, &c3)),
		Some(2),
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap_err();
	assert!(
		matches!(err_both, TransferError::EmptySelection),
		"expected EmptySelection, got: {err_both:?}"
	);
}
