//! #241: which two names are one folder or one file is the filesystem's to
//! say, under the write lock, never a table of ours. Consent is to a placed
//! folder (a `PlacedRoot`) and checked against what the disk holds after
//! this Apply made its folders; two writes to one file meet at
//! `create_new`. `name_key` only words a warning in the preview.
//!
//! Each case here is one the key once missed (case, Unicode's two forms,
//! the macOS data volume's firmlink, a character HFS+ ignores), and most of
//! them run again with the key switched off (`distinct_name_keys`): Apply
//! must hold either way.

use super::*;

fn tmp() -> (tempfile::TempDir, PathBuf) {
	let dir = tempfile::tempdir().unwrap();
	let path = dunce::canonicalize(dir.path()).unwrap();
	(dir, path)
}

/// Every path below `dir`, sorted, spelled as the filesystem lists it.
fn scan(dir: &Path) -> Vec<PathBuf> {
	fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
		for entry in fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			out.push(path.clone());
			if path.is_dir() && !path.is_symlink() {
				walk(&path, out);
			}
		}
	}
	let mut out = Vec::new();
	walk(dir, &mut out);
	out.sort();
	out
}

fn placed(path: &Path) -> PlacedRoot {
	PlacedRoot::resolve(None, path).unwrap()
}

/// The folders a mapping's routes are anchored in: its destination roots.
fn roots_of(mapping: &ImportMapping) -> Vec<PathBuf> {
	let mut roots: Vec<PathBuf> = mapping
		.primary_destination
		.iter()
		.chain(mapping.prefix_destinations.values())
		.map(|p| p.anchor().path().to_path_buf())
		.collect();
	roots.sort();
	roots.dedup();
	roots
}

fn plan(text: &str, mapping: &ImportMapping) -> TransferImportPlan {
	plan_import(text, "", &roots_of(mapping), mapping).unwrap()
}

/// Agrees to make every folder the user placed, and to overwrite.
fn agree_to_all(plan: &TransferImportPlan) -> RestoreSelection {
	RestoreSelection {
		overwrite_existing: true,
		confirmed_new_dirs: plan
			.destination_freshness()
			.placed_tails
			.iter()
			.flatten()
			.cloned()
			.chain(plan.new_dirs().requested)
			.collect(),
		..Default::default()
	}
}

fn expect_not_confirmed(result: Result<RestoreExecutionResult, TransferError>) {
	match result {
		Err(TransferError::NewDirsNotConfirmed { dirs }) => {
			assert!(!dirs.is_empty())
		}
		other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
	}
}

fn expect_collision(result: Result<RestoreExecutionResult, TransferError>) {
	match result {
		Err(TransferError::TargetCollision { .. }) => {}
		other => panic!("expected TargetCollision, got {other:?}"),
	}
}

/// Asserts that the filesystem under `dir` folds case.
#[cfg(any(target_os = "macos", windows))]
fn assert_folds_case(dir: &Path) {
	let probe = dir.join(".case-probe");
	fs::write(&probe, "").unwrap();
	assert!(
		dir.join(".CASE-PROBE").exists(),
		"needs a filesystem that folds case"
	);
	fs::remove_file(&probe).unwrap();
}

/// `/System/Volumes/Data` followed by `dir`: the macOS data volume's
/// firmlink, another spelling of the same folder.
#[cfg(target_os = "macos")]
fn data_volume_spelling(dir: &Path) -> PathBuf {
	let mut spelled = PathBuf::from("/System/Volumes/Data");
	spelled.push(dir.strip_prefix("/").unwrap());
	fs::metadata(&spelled).expect("the data volume's firmlink is there");
	spelled
}

/// A fresh HFS+ volume (`hdiutil`), detached when dropped. HFS+ stores
/// names decomposed and ignores some characters (U+200C among them), so
/// it merges names that APFS keeps apart.
#[cfg(target_os = "macos")]
struct Hfs {
	mount: PathBuf,
	_dir: tempfile::TempDir,
}

#[cfg(target_os = "macos")]
impl Hfs {
	/// `None` (after the `SNIP_REQUIRE_ALL_TESTS` check) when no volume can
	/// be made here.
	fn new() -> Option<Self> {
		use std::process::Command;
		let (dir, path) = tmp();
		let image = path.join("vol.dmg");
		let mount = path.join("mnt");
		fs::create_dir(&mount).unwrap();
		let made = Command::new("hdiutil")
			.args(["create", "-quiet", "-size", "8m", "-fs", "HFS+"])
			.args(["-volname", "snip241"])
			.arg(&image)
			.status()
			.is_ok_and(|s| s.success())
			&& Command::new("hdiutil")
				.args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
				.arg(&mount)
				.arg(&image)
				.status()
				.is_ok_and(|s| s.success());
		if !made {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"hdiutil could not make an HFS+ volume"
			);
			eprintln!("skipped: hdiutil could not make an HFS+ volume");
			return None;
		}
		Some(Self { mount, _dir: dir })
	}

	/// An empty workspace folder on the volume.
	fn workspace(&self) -> PathBuf {
		let ws = self.mount.join("ws");
		fs::create_dir(&ws).unwrap();
		dunce::canonicalize(ws).unwrap()
	}
}

#[cfg(target_os = "macos")]
impl Drop for Hfs {
	fn drop(&mut self) {
		let _ = std::process::Command::new("hdiutil")
			.args(["detach", "-quiet", "-force"])
			.arg(&self.mount)
			.status();
	}
}

const ZWNJ: &str = "\u{200C}";
const COMPOSED: &str = "\u{e9}";
const DECOMPOSED: &str = "e\u{301}";

// ---------------------------------------------------------------------------
// (a) HFS+: a character the filesystem ignores
// ---------------------------------------------------------------------------

/// `A/a.txt` and `A<U+200C>/a.txt` are one file on HFS+. The key does not
/// know that, so the preview shows two new files; Apply meets the second
/// at `create_new` and writes neither.
#[cfg(target_os = "macos")]
fn hfs_ignorable_character_collides() {
	let Some(hfs) = Hfs::new() else { return };
	let ws = hfs.workspace();
	let mapping =
		ImportMapping::with_primary(CanonicalRootId::new(&ws).unwrap());
	let plan = plan(
		&format!("// file: A/a.txt\nFIRST\n// file: A{ZWNJ}/a.txt\nSECOND\n"),
		&mapping,
	);
	assert_eq!(plan.create_operations().len(), 2);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

#[cfg(target_os = "macos")]
#[test]
fn a_character_hfs_ignores_cannot_write_one_file_twice() {
	hfs_ignorable_character_collides();
}

#[cfg(target_os = "macos")]
#[test]
fn a_character_hfs_ignores_cannot_write_one_file_twice_without_the_key() {
	let _keys = test_hooks::distinct_name_keys();
	hfs_ignorable_character_collides();
}

/// The destination `ws/A` does not exist; `q` is `ws`, and the payload
/// reaches `A` as `q/A<U+200C>`. Without consent the folder the user named
/// is not made, through whichever route.
#[cfg(target_os = "macos")]
fn hfs_ignorable_character_needs_consent() {
	let Some(hfs) = Hfs::new() else { return };
	let ws = hfs.workspace();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join("A")));
	mapping.map_prefix("q", CanonicalRootId::new(&ws).unwrap());
	let plan = plan(&format!("// file: q/A{ZWNJ}/a.txt\nA\n"), &mapping);
	expect_not_confirmed(plan.apply(&RestoreSelection::default()));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

#[cfg(target_os = "macos")]
#[test]
fn a_character_hfs_ignores_does_not_make_the_named_folder_without_consent() {
	hfs_ignorable_character_needs_consent();
}

#[cfg(target_os = "macos")]
#[test]
fn a_character_hfs_ignores_does_not_make_the_named_folder_without_the_key() {
	let _keys = test_hooks::distinct_name_keys();
	hfs_ignorable_character_needs_consent();
}

// ---------------------------------------------------------------------------
// (c) HFS+ stores a composed name decomposed: that is not a replacement
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn a_composed_folder_name_on_hfs_is_made_with_consent() {
	let Some(hfs) = Hfs::new() else { return };
	let ws = hfs.workspace();
	let mapping = ImportMapping::with_primary(placed(&ws.join(COMPOSED)));
	let plan = plan("// file: a.txt\nA\n", &mapping);
	let result = plan.apply(&agree_to_all(&plan)).unwrap();
	assert_eq!(result.created_count, 1);
	assert_eq!(result.created_dirs, 1);
	assert_eq!(
		fs::read_to_string(ws.join(COMPOSED).join("a.txt")).unwrap(),
		"A"
	);
}

// ---------------------------------------------------------------------------
// (b) The key switched off: every earlier bypass is still held by Apply
// ---------------------------------------------------------------------------

/// Case: the destination `ws/new` reached as `q/NEW`.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn without_the_key_a_case_alias_does_not_make_the_named_folder() {
	let (_tmp, ws) = tmp();
	assert_folds_case(&ws);
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join("new")));
	mapping.map_prefix("q", CanonicalRootId::new(&ws).unwrap());
	let plan = plan("// file: q/NEW/a.txt\nA\n", &mapping);
	expect_not_confirmed(plan.apply(&RestoreSelection::default()));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

/// Case: two routes to `new` and `NEW`, one file name in two cases.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn without_the_key_two_case_aliases_cannot_write_one_file_twice() {
	let (_tmp, ws) = tmp();
	assert_folds_case(&ws);
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join("other")));
	mapping.map_prefix("p", placed(&ws.join("new")));
	mapping.map_prefix("q", placed(&ws.join("NEW")));
	let plan = plan(
		"// file: p/a.txt\nFIRST\n// file: q/A.txt\nSECOND\n",
		&mapping,
	);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

/// Case: two rows overwrite one existing file through two cases.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn without_the_key_one_existing_file_is_not_overwritten_twice() {
	let (_tmp, ws) = tmp();
	assert_folds_case(&ws);
	fs::write(ws.join("x.txt"), "OLD").unwrap();
	let _keys = test_hooks::distinct_name_keys();
	let mapping =
		ImportMapping::with_primary(CanonicalRootId::new(&ws).unwrap());
	let plan =
		plan("// file: x.txt\nFIRST\n// file: X.txt\nSECOND\n", &mapping);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(fs::read_to_string(ws.join("x.txt")).unwrap(), "OLD");
}

/// Two hard links of one file: one file, whatever the names.
#[cfg(unix)]
#[test]
fn two_hard_links_of_one_file_are_not_overwritten_twice() {
	let (_tmp, ws) = tmp();
	fs::write(ws.join("a.txt"), "OLD").unwrap();
	fs::hard_link(ws.join("a.txt"), ws.join("b.txt")).unwrap();
	let mapping =
		ImportMapping::with_primary(CanonicalRootId::new(&ws).unwrap());
	let plan =
		plan("// file: a.txt\nFIRST\n// file: b.txt\nSECOND\n", &mapping);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "OLD");
	assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "OLD");
}

/// Unicode's two forms: the destination `ws/é` composed, reached as
/// `q/é` decomposed.
#[cfg(target_os = "macos")]
#[test]
fn without_the_key_the_other_unicode_form_does_not_make_the_named_folder() {
	let (_tmp, ws) = tmp();
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join(COMPOSED)));
	mapping.map_prefix("q", CanonicalRootId::new(&ws).unwrap());
	let plan = plan(&format!("// file: q/{DECOMPOSED}/a.txt\nA\n"), &mapping);
	expect_not_confirmed(plan.apply(&RestoreSelection::default()));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

/// Unicode's two forms: two routes to the two forms of one new folder.
#[cfg(target_os = "macos")]
#[test]
fn without_the_key_the_two_unicode_forms_cannot_write_one_file_twice() {
	let (_tmp, ws) = tmp();
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join("other")));
	mapping.map_prefix("p", placed(&ws.join(COMPOSED)));
	mapping.map_prefix("q", placed(&ws.join(DECOMPOSED)));
	let plan = plan(
		"// file: p/a.txt\nFIRST\n// file: q/a.txt\nSECOND\n",
		&mapping,
	);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

/// The firmlink: the destination `R/Sub/new`, reached through the data
/// volume's spelling of `R`.
#[cfg(target_os = "macos")]
#[test]
fn without_the_key_the_firmlink_does_not_make_the_named_folder() {
	let (_tmp, ws) = tmp();
	let root = ws.join("R");
	fs::create_dir_all(root.join("Sub")).unwrap();
	let spelled = data_volume_spelling(&root);
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping =
		ImportMapping::with_primary(placed(&root.join("Sub").join("new")));
	mapping.map_prefix("q", placed(&spelled));
	let plan = plan("// file: q/Sub/new/b.txt\nB\n", &mapping);
	expect_not_confirmed(plan.apply(&RestoreSelection::default()));
	assert_eq!(scan(&ws), vec![root.clone(), root.join("Sub")]);
}

/// The firmlink: two routes to one new folder through the two spellings
/// of the folder above it, one file name.
#[cfg(target_os = "macos")]
#[test]
fn without_the_key_the_firmlink_cannot_write_one_file_twice() {
	let (_tmp, ws) = tmp();
	let spelled = data_volume_spelling(&ws);
	let _keys = test_hooks::distinct_name_keys();
	let mut mapping = ImportMapping::with_primary(placed(&ws.join("new")));
	mapping.map_prefix("p", placed(&spelled.join("new")));
	let plan = plan(
		"// file: a.txt\nFIRST\n// file: p/a.txt\nSECOND\n",
		&mapping,
	);
	expect_collision(plan.apply(&agree_to_all(&plan)));
	assert_eq!(scan(&ws), Vec::<PathBuf>::new());
}

// ---------------------------------------------------------------------------
// (d) An absolute header naming a destination that is not made yet
// ---------------------------------------------------------------------------

/// `--repo ws/New` and a header `ws/new/a.txt`: one folder on a filesystem
/// that folds case, so the file lands in it, not under the whole path.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn an_absolute_header_in_another_case_lands_in_the_new_destination() {
	let (_tmp, ws) = tmp();
	assert_folds_case(&ws);
	let mapping = ImportMapping::with_primary(placed(&ws.join("New")));
	let header = ws.join("new").join("a.txt");
	let plan = plan(&format!("// file: {}\nA\n", header.display()), &mapping);
	plan.apply(&agree_to_all(&plan)).unwrap();
	assert_eq!(
		scan(&ws),
		vec![ws.join("New"), ws.join("New").join("a.txt")]
	);
	assert_eq!(fs::read_to_string(ws.join("New/a.txt")).unwrap(), "A");
}

/// `--repo ws/é` composed and a header with `é` decomposed.
#[cfg(target_os = "macos")]
#[test]
fn an_absolute_header_in_the_other_unicode_form_lands_in_the_new_destination() {
	let (_tmp, ws) = tmp();
	let mapping = ImportMapping::with_primary(placed(&ws.join(COMPOSED)));
	let header = ws.join(DECOMPOSED).join("a.txt");
	let plan = plan(&format!("// file: {}\nA\n", header.display()), &mapping);
	plan.apply(&agree_to_all(&plan)).unwrap();
	assert_eq!(
		scan(&ws),
		vec![ws.join(COMPOSED), ws.join(COMPOSED).join("a.txt")]
	);
}
