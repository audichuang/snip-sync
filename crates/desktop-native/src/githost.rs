//! Where the workbench's Git views read from.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use snip_core::gitsrc::GitError;
use snip_core::gitview::{LocalRepo, Read, RepoView};
use snip_core::workspace::RepoIdentity;
use snip_remote::Client;

use crate::WorkbenchModel;

/// Where the workbench's Git views read from.
#[derive(Clone)]
pub(crate) enum GitHost {
	Local,
	Remote {
		client: Arc<Client>,
		workspace: String,
		root: PathBuf, /* session.root */
	},
}

impl GitHost {
	/// Local: `known` -> LocalRepo::known (no process), else LocalRepo::open (one probe).
	/// Remote: no I/O at all (the first RepoView method call connects).
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
			Self::Remote {
				client,
				workspace,
				root: session_root,
			} => {
				let rel = self.remote_rel(root).ok_or_else(|| {
					format!(
						"repository '{}' is not under remote session root '{}'",
						root.display(),
						session_root.display()
					)
				})?;
				Ok(Box::new(snip_remote::RemoteRepo::new(
					client.clone(),
					workspace.clone(),
					rel,
				)))
			}
		}
	}

	/// `root` as the worker's share-relative "/"-joined path ("" for the share root);
	/// None when `root` is not under the session root, or has a non-Normal component.
	pub fn remote_rel(&self, root: &Path) -> Option<String> {
		match self {
			Self::Local => None,
			Self::Remote {
				root: session_root, ..
			} => crate::remote::remote_rel(session_root, root),
		}
	}
}

impl WorkbenchModel {
	pub(crate) fn git_host(&self) -> GitHost {
		if let Some(session) = &self.remote.session {
			GitHost::Remote {
				client: session.client.clone(),
				workspace: session.workspace.id.clone(),
				root: session.root.clone(),
			}
		} else {
			GitHost::Local
		}
	}
}

/// Translates Git errors encountered while querying refs into a displayable string.
/// For remote hosts exceeding the worker's frame limit (`GitError::OutputLimit`),
/// returns the localized message of `remote_refs_too_large`.
pub(crate) fn refs_error(
	host: &GitHost,
	e: GitError,
	locale: crate::i18n::Locale,
) -> String {
	if matches!(host, GitHost::Remote { .. })
		&& matches!(e, GitError::OutputLimit { .. })
	{
		crate::i18n::t("remote_refs_too_large", locale).to_string()
	} else {
		e.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use snip_core::gitview::ReadProfile;

	#[test]
	fn refs_error_mapping() {
		let client = std::sync::Arc::new(snip_remote::Client::new(
			snip_remote::RemoteHost::ssh("w"),
			"master".into(),
		));
		let remote_host = GitHost::Remote {
			client,
			workspace: "ws1".into(),
			root: PathBuf::from("snip-remote://fp/ws1"),
		};
		let local_host = GitHost::Local;

		// Remote + OutputLimit -> localized text of remote_refs_too_large (ZhTw and En)
		let zh_err = refs_error(
			&remote_host,
			GitError::OutputLimit {
				args: "remote view".into(),
				limit: 0,
			},
			crate::i18n::Locale::ZhTw,
		);
		assert_eq!(zh_err, "遠端參照資料過大");
		assert_eq!(
			zh_err,
			crate::i18n::t("remote_refs_too_large", crate::i18n::Locale::ZhTw)
		);
		let en_err = refs_error(
			&remote_host,
			GitError::OutputLimit {
				args: "remote view".into(),
				limit: 0,
			},
			crate::i18n::Locale::En,
		);
		assert_eq!(en_err, "Remote references too large");
		assert_eq!(
			en_err,
			crate::i18n::t("remote_refs_too_large", crate::i18n::Locale::En)
		);

		// Remote + other -> to_string()
		let other_err = GitError::Host("server died".into());
		let other_str = other_err.to_string();
		assert_eq!(
			refs_error(&remote_host, other_err, crate::i18n::Locale::En),
			other_str
		);

		// Local + OutputLimit -> to_string()
		let local_err = GitError::OutputLimit {
			args: "remote view".into(),
			limit: 0,
		};
		let local_str = local_err.to_string();
		assert_eq!(
			refs_error(&local_host, local_err, crate::i18n::Locale::En),
			local_str
		);
	}

	#[test]
	fn local_host_refuses_a_remote_key() {
		if !crate::run_isolated(
			"githost::tests::local_host_refuses_a_remote_key",
		) {
			return;
		}
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

	#[test]
	fn remote_host_opens_without_io_and_maps_roots() {
		if !crate::run_isolated(
			"githost::tests::remote_host_opens_without_io_and_maps_roots",
		) {
			return;
		}
		let client = std::sync::Arc::new(snip_remote::Client::new(
			snip_remote::RemoteHost::ssh("w"),
			"master".into(),
		));
		let session_root = std::path::PathBuf::from("snip-remote://fp/ws1");
		let host = GitHost::Remote {
			client,
			workspace: "ws1".into(),
			root: session_root.clone(),
		};

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let before_flight = snip_core::gitrun::in_flight();
		let before_queued = snip_core::gitrun::queued();

		// remote_rel cases
		assert_eq!(host.remote_rel(&session_root), Some(String::new()));
		assert_eq!(
			host.remote_rel(&session_root.join("sub").join("repo")),
			Some("sub/repo".to_string())
		);
		assert_eq!(host.remote_rel(Path::new("/outside")), None);
		assert_eq!(GitHost::Local.remote_rel(&session_root), None);

		// open returns Ok for a root under session.root and Err for a path outside
		let under = host.open(&session_root, None, &read);
		assert!(under.is_ok());

		let sub = host.open(&session_root.join("sub"), None, &read);
		assert!(sub.is_ok());

		let outside = host.open(Path::new("/outside"), None, &read);
		assert!(outside.is_err());

		// GitLoad/gitrun::in_flight unchanged; no worker needed since no I/O happens
		assert_eq!(snip_core::gitrun::in_flight(), before_flight);
		assert_eq!(snip_core::gitrun::queued(), before_queued);
	}
}
