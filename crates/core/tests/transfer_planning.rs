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
use snip_core::gitsrc::{self, Git, GitSource};
use snip_core::restore::{self, RestoreSelection, SkipReason};
use snip_core::settings::{FilterAction, FilterRule, FilterType, Settings};
use snip_core::transfer::{
	detect_clipboard_prefixes, plan_commit_export, plan_export, plan_import,
	validate_commit_selection, CanonicalRootId, DestinationFreshnessSnapshot,
	ExportItem, ExportSelection, ImportMapping, SourceFreshnessSnapshot,
	SourceKind, TransferError,
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
		let repo_path = dir.path().join(name);
		fs::create_dir_all(&repo_path).unwrap();
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
			},
			ExportItem {
				root: src2.canonical_id(),
				relative_path: "src/lib.rs".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
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
			},
			ExportItem {
				root: repo2.canonical_id(),
				relative_path: "file2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file.txt".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
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
			},
			ExportItem {
				root: r.canonical_id(),
				relative_path: "f2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
			},
			ExportItem {
				root: src.canonical_id(),
				relative_path: "deleted_later.txt".to_string(),
				source: SourceKind::Commit { rev: c2 },
				change_type: Some(ChangeType::Deleted),
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
			assert_eq!(path, dst.path().join("target.txt"));
			assert!(msg.contains("multiple operations target"));
		}
		other => panic!("expected TargetCollision, got {other:?}"),
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/ignore.tmp".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "file_b.txt".to_string(),
				source: SourceKind::Commit {
					rev: "HEAD~1".to_string(),
				},
				change_type: Some(ChangeType::Modified),
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
	};
	let err2 = ExportSelection::new(
		vec![r1.path().to_path_buf()],
		None,
		vec![undeclared_item],
	)
	.unwrap_err();
	match err2 {
		TransferError::UnknownRoot(p) => assert_eq!(p, r2.path()),
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
		TransferError::UnknownRoot(p) => assert_eq!(p, r2.path()),
		other => panic!("expected UnknownRoot, got: {other:?}"),
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "src/b.rs".to_string(),
				source: SourceKind::File,
				change_type: None,
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "new.txt".to_string(),
				source: SourceKind::Staged,
				change_type: Some(ChangeType::New),
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "mod.txt".to_string(),
				source: SourceKind::Commit { rev: commit_oid },
				change_type: Some(ChangeType::Modified),
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "valid.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "big2.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
		.recv_timeout(std::time::Duration::from_secs(2))
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "regular.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
		.recv_timeout(std::time::Duration::from_secs(2))
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "ok.txt".to_string(),
				source: SourceKind::Working,
				change_type: None,
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
			},
			ExportItem {
				root: repo.canonical_id(),
				relative_path: "b.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
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
		}],
	)
	.unwrap();

	let plan = plan_export(&selection, &settings, None).unwrap();

	// Graph empty payload omits source_root marker line!
	assert!(!plan.payload.contains("# source_root:"));
	assert_eq!(plan.payload, gitsrc_res.payload);
	assert_eq!(plan.files, gitsrc_res.files);
}
