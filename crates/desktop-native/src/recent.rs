//! Remembered workspaces: IntelliJ's recent-projects list, and the one the
//! app reopens when it is launched without `--workspace` (Finder, Explorer).

use std::path::{Path, PathBuf};

const CAP: usize = 10;

/// The user's home folder.
pub fn home() -> Option<PathBuf> {
	std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
		.map(PathBuf::from)
}

/// `path` with the home folder shown as `~`, as IntelliJ lists projects.
pub fn tilde(path: &Path) -> String {
	match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf))
	{
		Some(rel) if !rel.as_os_str().is_empty() => {
			format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display())
		}
		_ => path.display().to_string(),
	}
}

/// The app's config folder. `SNIP_CONFIG_DIR` overrides it; an e2e run
/// without the override, and every in-process test, never reads or writes
/// the user's files.
pub fn config_dir() -> Option<PathBuf> {
	if cfg!(test) {
		return None;
	}
	if std::env::var_os("SNIP_CONFIG_DIR").is_none() && crate::e2e_on() {
		return None;
	}
	snip_remote::default_config_dir()
}

/// Where the list lives.
fn file() -> Option<PathBuf> {
	Some(config_dir()?.join("recent-workspaces.json"))
}

/// Remembered workspaces, newest first.
pub fn load() -> Vec<PathBuf> {
	file().map(|f| load_from(&f)).unwrap_or_default()
}

/// Moves `path` to the front of the saved list, refreshes `list` from it
/// and saves it. Every workspace tab keeps its own copy, so the file is
/// read first: another tab's entry is not written over. A failed write
/// only loses the list, never the workspace.
pub fn remember(list: &mut Vec<PathBuf>, path: &Path) {
	let Some(f) = file() else {
		push_front(list, path);
		return;
	};
	remember_in(&f, list, path);
}

fn remember_in(f: &Path, list: &mut Vec<PathBuf>, path: &Path) {
	*list = load_from(f);
	push_front(list, path);
	save_to(f, list);
}

/// Drops folders that no longer exist.
fn load_from(f: &Path) -> Vec<PathBuf> {
	let Ok(bytes) = std::fs::read(f) else {
		return Vec::new();
	};
	let list: Vec<PathBuf> = serde_json::from_slice(&bytes).unwrap_or_default();
	list.into_iter().filter(|p| p.is_dir()).take(CAP).collect()
}

fn push_front(list: &mut Vec<PathBuf>, path: &Path) {
	list.retain(|p| p != path);
	list.insert(0, path.to_path_buf());
	list.truncate(CAP);
}

fn save_to(f: &Path, list: &[PathBuf]) {
	if let Some(dir) = f.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	if let Ok(bytes) = serde_json::to_vec_pretty(list) {
		let _ = std::fs::write(f, bytes);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A `#[gpui::test]` that opens a workspace calls `remember`; it must
	/// not land in the user's recent list.
	#[test]
	fn in_process_tests_never_touch_the_users_config() {
		assert_eq!(config_dir(), None);
		assert_eq!(file(), None);
	}

	#[test]
	fn newest_first_capped_and_missing_folders_dropped() {
		let tmp = tempfile::tempdir().unwrap();
		let dirs: Vec<PathBuf> = (0..12)
			.map(|i| {
				let d = tmp.path().join(format!("ws{i}"));
				std::fs::create_dir(&d).unwrap();
				d
			})
			.collect();
		let mut list = Vec::new();
		for d in &dirs {
			push_front(&mut list, d);
		}
		push_front(&mut list, &dirs[5]);
		assert_eq!(list.len(), CAP);
		assert_eq!(list[0], dirs[5]);
		assert_eq!(list[1], dirs[11]);
		assert_eq!(list.iter().filter(|p| **p == dirs[5]).count(), 1);

		let f = tmp.path().join("cfg/recent-workspaces.json");
		save_to(&f, &list);
		std::fs::remove_dir(&dirs[11]).unwrap();
		let loaded = load_from(&f);
		assert_eq!(loaded[0], dirs[5]);
		assert!(!loaded.contains(&dirs[11]));
		assert_eq!(loaded.len(), CAP - 1);
		assert!(load_from(&tmp.path().join("missing.json")).is_empty());
	}

	/// Two workspace tabs each hold a copy of the list; the second open
	/// must keep the first one's entry.
	#[test]
	fn a_second_tab_keeps_the_first_tabs_entry() {
		let tmp = tempfile::tempdir().unwrap();
		let a = tmp.path().join("a");
		let b = tmp.path().join("b");
		std::fs::create_dir(&a).unwrap();
		std::fs::create_dir(&b).unwrap();
		let f = tmp.path().join("recent-workspaces.json");
		let mut tab_one = Vec::new();
		let mut tab_two = Vec::new();
		remember_in(&f, &mut tab_one, &a);
		remember_in(&f, &mut tab_two, &b);
		assert_eq!(load_from(&f), vec![b.clone(), a.clone()]);
		assert_eq!(tab_two, vec![b, a]);
	}
}
