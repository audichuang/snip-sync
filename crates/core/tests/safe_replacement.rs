//! Replacement writes must not follow planted links or extend the target name.

use snip_core::fsutil::write_text_file;
use std::fs;

fn root(dir: &std::path::Path) -> std::path::PathBuf {
	let real = dir.join("real");
	fs::create_dir(&real).unwrap();
	#[cfg(unix)]
	{
		let link = dir.join("root");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		link
	}
	#[cfg(not(unix))]
	real
}

#[cfg(unix)]
fn assert_planted_temp_link_is_ignored(name: &str, symlink: bool) {
	let dir = tempfile::tempdir().unwrap();
	let root = root(dir.path());
	let outside = dir.path().join("outside.txt");
	fs::write(&outside, "keep me").unwrap();
	// This test binary performs fewer than eight replacement writes,
	// regardless of test order: cover every old counter-based name.
	for n in 0..8 {
		let planted =
			root.join(format!(".{name}.snip-write.{n}.{}", std::process::id()));
		if symlink {
			std::os::unix::fs::symlink(&outside, &planted).unwrap();
		} else {
			fs::hard_link(&outside, &planted).unwrap();
		}
	}
	write_text_file(&root.join(name), "pasted").unwrap();
	assert_eq!(fs::read_to_string(&outside).unwrap(), "keep me", "{name}");
	assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "pasted");
}

#[cfg(unix)]
#[test]
fn write_ignores_a_planted_temp_symlink() {
	assert_planted_temp_link_is_ignored("symlink.txt", true);
}

#[cfg(unix)]
#[test]
fn write_ignores_a_planted_temp_hard_link() {
	assert_planted_temp_link_is_ignored("hardlink.txt", false);
}

#[test]
fn write_creates_and_overwrites_a_255_byte_name() {
	let dir = tempfile::tempdir().unwrap();
	let root = root(dir.path());
	let path = root.join("n".repeat(255));
	write_text_file(&path, "created").unwrap();
	assert_eq!(fs::read_to_string(&path).unwrap(), "created");
	write_text_file(&path, "overwritten").unwrap();
	assert_eq!(fs::read_to_string(&path).unwrap(), "overwritten");
	assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
}

#[test]
fn failed_replacement_removes_the_temp_file() {
	let dir = tempfile::tempdir().unwrap();
	let root = root(dir.path());
	let path = root.join("directory");
	fs::create_dir(&path).unwrap();
	assert!(write_text_file(&path, "cannot replace a directory").is_err());
	assert!(path.is_dir());
	assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
}
