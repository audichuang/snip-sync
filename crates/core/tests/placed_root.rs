//! #241: a paste folder that may not exist yet (`PlacedRoot`). The nearest
//! existing ancestor is the anchor; the names below it are the tail.
//! `resolve` only reads: nothing here is made on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use snip_core::reason::Reason;
use snip_core::transfer::{CanonicalRootId, DestinationError, PlacedRoot};

fn workspace() -> (tempfile::TempDir, PathBuf) {
	let tmp = tempfile::tempdir().unwrap();
	let ws = dunce::canonicalize(tmp.path()).unwrap().join("ws");
	fs::create_dir_all(&ws).unwrap();
	(tmp, ws)
}

/// Every path below `dir`, sorted: two scans must be equal when nothing was
/// written in between.
fn scan(dir: &Path) -> Vec<PathBuf> {
	fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
		for entry in fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			out.push(path.clone());
			if path.is_dir() {
				walk(&path, out);
			}
		}
	}
	let mut out = Vec::new();
	walk(dir, &mut out);
	out.sort();
	out
}

fn git_init(dir: &Path) {
	let out = Command::new("git")
		.args(["init", "-q"])
		.current_dir(dir)
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.unwrap();
	assert!(out.status.success());
}

#[test]
fn p1_an_existing_folder_has_no_tail() {
	let (_tmp, ws) = workspace();
	let placed = PlacedRoot::resolve(None, &ws).unwrap();
	assert!(placed.tail().is_empty());
	assert_eq!(placed.tail_rel(), "");
	assert_eq!(placed.path(), ws);
	assert!(placed.new_chain().is_empty());
	assert_eq!(placed, PlacedRoot::from(CanonicalRootId::new(&ws).unwrap()));
}

#[test]
fn p2_a_missing_folder_is_its_anchor_and_a_tail_and_nothing_is_made() {
	let (_tmp, ws) = workspace();
	let before = scan(&ws);
	let placed =
		PlacedRoot::resolve(None, &ws.join("new").join("deeper")).unwrap();
	assert_eq!(placed.anchor().path(), ws);
	assert_eq!(placed.tail(), ["new", "deeper"]);
	assert_eq!(placed.tail_rel(), "new/deeper");
	assert_eq!(placed.path(), ws.join("new").join("deeper"));
	assert_eq!(
		placed.new_chain(),
		vec![ws.join("new"), ws.join("new").join("deeper")]
	);
	assert_eq!(scan(&ws), before, "resolve must not write");
}

#[test]
fn p3_a_file_on_the_way_is_not_a_folder() {
	let (_tmp, ws) = workspace();
	fs::write(ws.join("file"), "x").unwrap();
	match PlacedRoot::resolve(None, &ws.join("file").join("new")) {
		Err(DestinationError::NotADirectory { path }) => {
			assert_eq!(path, ws.join("file"))
		}
		other => panic!("expected NotADirectory, got {other:?}"),
	}
	match PlacedRoot::resolve(None, &ws.join("file")) {
		Err(DestinationError::NotADirectory { path }) => {
			assert_eq!(path, ws.join("file"))
		}
		other => panic!("expected NotADirectory, got {other:?}"),
	}
}

#[test]
fn p4_a_parent_segment_in_the_missing_part_is_refused() {
	let (_tmp, ws) = workspace();
	let path = ws.join("new").join("..").join("x");
	assert!(matches!(
		PlacedRoot::resolve(None, &path),
		Err(DestinationError::InvalidName { .. })
	));
}

#[test]
fn p5_names_that_cannot_be_a_new_folder_are_refused_on_every_platform() {
	let (_tmp, ws) = workspace();
	for name in [
		".git", ".GIT.", ".git ", "con", "NUL.txt", "Com1", "lpt9.log", "a.",
		"a ", "a:b",
	] {
		let result = PlacedRoot::resolve(None, &ws.join(name));
		assert!(
			matches!(result, Err(DestinationError::InvalidName { .. })),
			"{name:?} gave {result:?}"
		);
	}
	// A name that only looks alike is fine.
	for name in ["console", "a.b", "nul0", "com", "lpt10", ".gitignore-dir"] {
		let placed = PlacedRoot::resolve(None, &ws.join(name)).unwrap();
		assert_eq!(placed.tail(), [name]);
	}
}

#[test]
fn p6_a_path_that_leaves_the_bound_is_outside() {
	let (tmp, ws) = workspace();
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&outside).unwrap();
	let path = ws.join("..").join("outside").join("new");
	assert!(matches!(
		PlacedRoot::resolve(Some(&ws), &path),
		Err(DestinationError::OutsideBound { .. })
	));
	// Inside the bound it resolves.
	assert!(PlacedRoot::resolve(Some(&ws), &ws.join("new")).is_ok());
	// A folder that is the bound itself is inside it.
	assert!(PlacedRoot::resolve(Some(&ws), &ws).is_ok());
}

#[cfg(unix)]
#[test]
fn p7_a_dangling_symlink_is_not_a_folder() {
	let (_tmp, ws) = workspace();
	std::os::unix::fs::symlink(ws.join("gone"), ws.join("dang")).unwrap();
	match PlacedRoot::resolve(None, &ws.join("dang").join("new")) {
		Err(DestinationError::NotADirectory { path }) => {
			assert_eq!(path, ws.join("dang"))
		}
		other => panic!("expected NotADirectory, got {other:?}"),
	}
}

#[cfg(unix)]
#[test]
fn p8_a_root_spelled_through_a_symlink_resolves_to_the_real_folder() {
	let tmp = tempfile::tempdir().unwrap();
	let base = dunce::canonicalize(tmp.path()).unwrap();
	let real = base.join("real");
	fs::create_dir_all(&real).unwrap();
	let link = base.join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();
	let placed = PlacedRoot::resolve(Some(&link), &link.join("new")).unwrap();
	assert_eq!(placed.path(), real.join("new"));
	assert_eq!(placed.anchor().path(), real);
}

#[cfg(unix)]
#[test]
fn p9_an_anchor_that_a_symlink_carries_out_of_the_bound_is_outside() {
	let (tmp, ws) = workspace();
	let elsewhere = tmp.path().join("elsewhere");
	fs::create_dir_all(&elsewhere).unwrap();
	std::os::unix::fs::symlink(&elsewhere, ws.join("out")).unwrap();
	assert!(matches!(
		PlacedRoot::resolve(Some(&ws), &ws.join("out").join("new")),
		Err(DestinationError::OutsideBound { .. })
	));
}

#[test]
fn p10_nothing_new_goes_into_a_git_directory() {
	let (_tmp, ws) = workspace();
	let repo = ws.join("repo");
	fs::create_dir_all(&repo).unwrap();
	git_init(&repo);
	assert!(matches!(
		PlacedRoot::resolve(None, &repo.join(".git").join("new")),
		Err(DestinationError::InvalidName { .. })
	));
	// A missing folder next to it is fine.
	assert!(PlacedRoot::resolve(None, &repo.join("new")).is_ok());
}

#[test]
fn p11_each_failure_keeps_its_own_reason() {
	let outside = DestinationError::OutsideBound { path: "/x".into() };
	assert!(matches!(outside.reason(), Reason::OutsideRoot { .. }));
	let not_dir = DestinationError::NotADirectory { path: "/f".into() };
	assert!(matches!(not_dir.reason(), Reason::NotRegularFile { .. }));
	let invalid = DestinationError::InvalidName { name: "nul".into() };
	assert!(matches!(invalid.reason(), Reason::UnsafePath { .. }));
	// It survives being an io::Error, which is what PasteError carries.
	let wrapped: std::io::Error =
		DestinationError::InvalidName { name: "nul".into() }.into();
	assert!(wrapped
		.get_ref()
		.and_then(|e| e.downcast_ref::<DestinationError>())
		.is_some());
}
