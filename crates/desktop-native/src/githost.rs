//! Where the workbench's Git views read from.

use std::path::Path;

use snip_core::gitview::{LocalRepo, Read, RepoView};
use snip_core::workspace::RepoIdentity;

use crate::WorkbenchModel;

/// Where the workbench's Git views read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GitHost {
	Local,
}

impl GitHost {
	/// Local: `known` -> LocalRepo::known (no process), else LocalRepo::open (one probe).
	pub fn open(
		&self,
		root: &Path,
		known: Option<&RepoIdentity>,
		read: &Read,
	) -> Result<Box<dyn RepoView>, String> {
		match self {
			Self::Local => {
				if root.to_string_lossy().starts_with("snip-remote://") {
					return Err(format!(
						"local Git host cannot open remote repository '{}'",
						root.display()
					));
				}
				match known {
					Some(id) => Ok(Box::new(LocalRepo::known(id))),
					None => LocalRepo::open(root, read)
						.map(|r| Box::new(r) as Box<dyn RepoView>)
						.map_err(|e| e.to_string()),
				}
			}
		}
	}
}

impl WorkbenchModel {
	pub(crate) fn git_host(&self) -> GitHost {
		GitHost::Local
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use snip_core::gitview::ReadProfile;

	#[test]
	fn local_host_refuses_a_remote_key() {
		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let before_flight = snip_core::gitrun::in_flight();
		let before_queued = snip_core::gitrun::queued();
		let res =
			GitHost::Local.open(Path::new("snip-remote://x/y"), None, &read);
		assert!(res.is_err());
		assert_eq!(snip_core::gitrun::in_flight(), before_flight);
		assert_eq!(snip_core::gitrun::queued(), before_queued);

		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("r");
		std::fs::create_dir(&root).unwrap();
		crate::paste::tests::git_init(&root);
		let id = RepoIdentity::resolve(
			&snip_core::gitsrc::Git::open(&root).unwrap(),
			&snip_core::gitrun::RunOptions::default(),
		)
		.unwrap();

		let before_flight = snip_core::gitrun::in_flight();
		let before_queued = snip_core::gitrun::queued();
		let res =
			GitHost::Local.open(Path::new("/nonexistent"), Some(&id), &read);
		assert!(res.is_ok());
		assert_eq!(snip_core::gitrun::in_flight(), before_flight);
		assert_eq!(snip_core::gitrun::queued(), before_queued);
	}

	#[test]
	fn local_host_open_uses_identity_and_opens_unknown_root() {
		let t = tempfile::tempdir().unwrap();
		let r = t.path().join("r");
		std::fs::create_dir(&r).unwrap();
		crate::paste::tests::git_init(&r);
		let id = RepoIdentity::resolve(
			&snip_core::gitsrc::Git::open(&r).unwrap(),
			&snip_core::gitrun::RunOptions::default(),
		)
		.unwrap();
		#[cfg(unix)]
		let alias = {
			let link = t.path().join("link");
			std::os::unix::fs::symlink(&r, &link).unwrap();
			link
		};
		#[cfg(not(unix))]
		let alias = {
			std::fs::create_dir(r.join("x")).unwrap();
			r.join("x").join("..")
		};
		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let from_alias = GitHost::Local.open(&alias, None, &read).unwrap();
		assert!(from_alias.change_list(10, &read).is_ok());

		let from_known = GitHost::Local
			.open(Path::new("/nonexistent"), Some(&id), &read)
			.unwrap();
		assert!(from_known.change_list(10, &read).is_ok());
	}
}
