//! The workspace tabs a normal launch reopens: `open-tabs.json` in the
//! config folder, written whenever a tab opens, closes or is switched to.
//!
//! It replaces `remote-last.json`, which named only the last workspace and
//! only when it was remote: the first launch that finds no `open-tabs.json`
//! turns that file into one remote tab and deletes it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::remote::RecentRemote;
use crate::tabs::WsIdentity;

const FILE: &str = "open-tabs.json";
const LEGACY_FILE: &str = "remote-last.json";

/// One saved tab: a canonical local folder, or an ssh Host alias with the
/// worker's real path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedTab {
	Local { path: PathBuf },
	Remote { host: String, path: String },
}

impl SavedTab {
	pub fn of(identity: &WsIdentity) -> Self {
		match identity {
			WsIdentity::Local(path) => SavedTab::Local { path: path.clone() },
			WsIdentity::Remote { host, path } => SavedTab::Remote {
				host: host.clone(),
				path: path.clone(),
			},
		}
	}
}

/// The tabs in order, and which one was shown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTabs {
	pub tabs: Vec<SavedTab>,
	/// Index into `tabs`; none when the shown tab was empty or no tab was
	/// left.
	pub active: Option<usize>,
}

impl OpenTabs {
	/// The tabs of the old `remote-last.json`: its one remote folder.
	fn from_legacy(last: RecentRemote) -> Self {
		OpenTabs {
			tabs: vec![SavedTab::Remote {
				host: last.host,
				path: last.path,
			}],
			active: Some(0),
		}
	}
}

/// The config folder a normal launch restores from and saves to; none
/// under `cfg(test)` and in an e2e run without `SNIP_CONFIG_DIR`.
pub fn store_dir() -> Option<PathBuf> {
	crate::recent::config_dir()
}

/// The saved tabs in `dir`. None when nothing was ever saved there (nor a
/// `remote-last.json` left to turn into tabs) or the file cannot be read:
/// the launch then falls back to the recent list. A `remote-last.json`
/// is converted, saved as `open-tabs.json` and deleted.
pub fn load_from(dir: &Path) -> Option<OpenTabs> {
	let file = dir.join(FILE);
	let legacy = dir.join(LEGACY_FILE);
	if file.exists() {
		let _ = std::fs::remove_file(&legacy);
		let bytes = std::fs::read(&file).ok()?;
		let mut tabs: OpenTabs = serde_json::from_slice(&bytes).ok()?;
		if tabs.active.is_some_and(|a| a >= tabs.tabs.len()) {
			tabs.active = None;
		}
		return Some(tabs);
	}
	let last: RecentRemote =
		serde_json::from_slice(&std::fs::read(&legacy).ok()?).ok()?;
	let tabs = OpenTabs::from_legacy(last);
	if save_to(dir, &tabs).is_ok() {
		let _ = std::fs::remove_file(&legacy);
	}
	Some(tabs)
}

/// Writes `tabs` to `dir`. A failed write only loses the restore.
pub fn save_to(dir: &Path, tabs: &OpenTabs) -> std::io::Result<()> {
	snip_remote::save_json(&dir.join(FILE), tabs)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn in_process_tests_have_no_store() {
		assert_eq!(store_dir(), None);
	}

	#[test]
	fn saved_tabs_round_trip_in_order_with_the_shown_one() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = tmp.path().join("cfg");
		assert_eq!(load_from(&dir), None, "nothing saved yet");
		let tabs = OpenTabs {
			tabs: vec![
				SavedTab::Local {
					path: PathBuf::from("/w/a"),
				},
				SavedTab::Remote {
					host: "box".into(),
					path: "/srv/b".into(),
				},
			],
			active: Some(1),
		};
		save_to(&dir, &tabs).unwrap();
		assert_eq!(load_from(&dir), Some(tabs));
		let empty = OpenTabs::default();
		save_to(&dir, &empty).unwrap();
		assert_eq!(load_from(&dir), Some(empty), "no tab is not nothing saved");
	}

	#[test]
	fn the_file_names_each_tab_by_kind() {
		let tmp = tempfile::tempdir().unwrap();
		let tabs = OpenTabs {
			tabs: vec![SavedTab::Remote {
				host: "box".into(),
				path: "/srv/b".into(),
			}],
			active: Some(0),
		};
		save_to(tmp.path(), &tabs).unwrap();
		let json: serde_json::Value = serde_json::from_slice(
			&std::fs::read(tmp.path().join(FILE)).unwrap(),
		)
		.unwrap();
		assert_eq!(
			json,
			serde_json::json!({
				"tabs": [{"kind": "remote", "host": "box", "path": "/srv/b"}],
				"active": 0,
			})
		);
	}

	#[test]
	fn the_old_remote_last_file_becomes_one_remote_tab_and_is_deleted() {
		let tmp = tempfile::tempdir().unwrap();
		let legacy = tmp.path().join(LEGACY_FILE);
		std::fs::write(&legacy, br#"{"host":"macmini","path":"/Users/x/cat"}"#)
			.unwrap();
		let want = OpenTabs {
			tabs: vec![SavedTab::Remote {
				host: "macmini".into(),
				path: "/Users/x/cat".into(),
			}],
			active: Some(0),
		};
		assert_eq!(load_from(tmp.path()), Some(want.clone()));
		assert!(!legacy.exists());
		assert!(tmp.path().join(FILE).exists());
		assert_eq!(load_from(tmp.path()), Some(want), "converted once");
	}

	#[test]
	fn a_saved_list_wins_over_a_leftover_remote_last_file() {
		let tmp = tempfile::tempdir().unwrap();
		save_to(tmp.path(), &OpenTabs::default()).unwrap();
		let legacy = tmp.path().join(LEGACY_FILE);
		std::fs::write(&legacy, br#"{"host":"h","path":"/p"}"#).unwrap();
		assert_eq!(load_from(tmp.path()), Some(OpenTabs::default()));
		assert!(!legacy.exists());
	}

	#[test]
	fn an_unreadable_file_or_a_stale_index_restores_nothing_wrong() {
		let tmp = tempfile::tempdir().unwrap();
		std::fs::write(tmp.path().join(FILE), b"{not json").unwrap();
		assert_eq!(load_from(tmp.path()), None);
		std::fs::write(
			tmp.path().join(FILE),
			br#"{"tabs":[{"kind":"local","path":"/w"}],"active":3}"#,
		)
		.unwrap();
		assert_eq!(load_from(tmp.path()).unwrap().active, None);
		std::fs::write(tmp.path().join(LEGACY_FILE), b"garbage").unwrap();
		std::fs::remove_file(tmp.path().join(FILE)).unwrap();
		assert_eq!(load_from(tmp.path()), None);
	}
}
