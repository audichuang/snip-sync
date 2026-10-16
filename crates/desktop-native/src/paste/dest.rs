//! The folder a paste preview writes into, as the user picked it (#238).
//!
//! The session keeps it apart from the plan: a plan is rebuilt, disarmed
//! and refused over its life, while the folder is what the user chose and
//! must survive all of that.

use std::path::{Component, Path, PathBuf};

use crate::i18n::Msg;

/// The destination folder of the open paste preview: a tree path (a local
/// path, or a path under the remote session root) and where it lies in the
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestFolder {
	pub path: PathBuf,
	/// Workspace-relative spelling, "/"-separated. "" is the workspace
	/// root itself; `None` is a folder outside the workspace (an enclosing
	/// repo, a `--restore-dir` elsewhere), shown as one plain label.
	pub rel: Option<String>,
}

impl DestFolder {
	pub fn new(
		workspace_root: &Path,
		path: PathBuf,
		remote: bool,
	) -> DestFolder {
		let rel = folder_rel(workspace_root, &path, remote);
		DestFolder { path, rel }
	}
}

/// The destination editor of the panel (the typed path field) while it is
/// open. It is an inline layer of the `OverlayStack` (#226), whose layer
/// holds this state: Escape and the focus it gives back follow the stack's
/// one rule, and every path that closes the preview drops the layer.
#[derive(Debug, Clone, Default)]
pub struct DestEdit {
	/// Why the last typed path was refused; the editor stays open on it.
	pub error: Option<Msg>,
}

/// `path` relative to `workspace_root`, the way the breadcrumb and the
/// "paste into" menu name it: "" for the workspace root, `None` when it lies
/// outside or a part is not UTF-8 text.
///
/// Compared by words first. A local path that does not start with the
/// workspace by words (the workspace is spelled through a symlink, as macOS
/// `/var` for `/private/var` is) is compared again as resolved paths, unless it
/// is a plain ancestor of the workspace, which is outside however it is
/// spelled and costs no system call. A remote tree path
/// is virtual, so only the words count.
pub fn folder_rel(
	workspace_root: &Path,
	path: &Path,
	remote: bool,
) -> Option<String> {
	if remote {
		return crate::remote::remote_rel(workspace_root, path);
	}
	lexical_rel(workspace_root, path).or_else(|| {
		// A folder above the workspace (an enclosing repo) is outside it
		// however either is spelled: the bar asks this every frame, and
		// that case must cost no system call.
		if workspace_root.starts_with(path) {
			return None;
		}
		let root = dunce::canonicalize(workspace_root).ok()?;
		let path = dunce::canonicalize(path).ok()?;
		lexical_rel(&root, &path)
	})
}

fn lexical_rel(root: &Path, path: &Path) -> Option<String> {
	let rel = path.strip_prefix(root).ok()?;
	let mut parts = Vec::new();
	for part in rel.components() {
		match part {
			Component::Normal(name) => parts.push(name.to_str()?),
			_ => return None,
		}
	}
	Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn folder_rel_reads_the_words_of_a_path_under_the_workspace() {
		let ws = Path::new("/w/ws");
		assert_eq!(folder_rel(ws, ws, false).as_deref(), Some(""));
		assert_eq!(
			folder_rel(ws, &ws.join("web").join("src"), false).as_deref(),
			Some("web/src")
		);
		assert_eq!(
			folder_rel(ws, &ws.join("my dir"), false).as_deref(),
			Some("my dir")
		);
	}

	#[test]
	fn folder_rel_is_none_outside_the_workspace_or_with_dots() {
		let ws = Path::new("/w/ws");
		assert_eq!(folder_rel(ws, Path::new("/w"), false), None);
		assert_eq!(folder_rel(ws, Path::new("/w/ws2/x"), false), None);
		assert_eq!(folder_rel(ws, Path::new("/w/ws/../x"), false), None);
		// A `.` is no part of a path: `/w/ws/./x` is `/w/ws/x`.
		assert_eq!(
			folder_rel(ws, Path::new("/w/ws/./x"), false).as_deref(),
			Some("x")
		);
	}

	#[test]
	fn folder_rel_of_a_remote_tree_path_counts_only_its_words() {
		let root = Path::new("snip-remote://h/ws");
		assert_eq!(folder_rel(root, root, true).as_deref(), Some(""));
		assert_eq!(
			folder_rel(root, &root.join("a").join("b"), true).as_deref(),
			Some("a/b")
		);
		assert_eq!(folder_rel(root, Path::new("snip-remote://h"), true), None);
		// Nothing on this machine's disk is consulted for a remote path.
		assert_eq!(folder_rel(root, Path::new("/tmp"), true), None);
	}

	/// The workspace spelled through a symlink while the folder is spelled
	/// by its real path (what a scan reports), as `/var` and `/private/var`.
	#[cfg(unix)]
	#[test]
	fn folder_rel_resolves_a_workspace_spelled_through_a_symlink() {
		let tmp = tempfile::tempdir().unwrap();
		let real = tmp.path().join("real");
		std::fs::create_dir_all(real.join("sub")).unwrap();
		let link = tmp.path().join("link");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		let real = dunce::canonicalize(&real).unwrap();
		assert_eq!(
			folder_rel(&link, &real.join("sub"), false).as_deref(),
			Some("sub")
		);
		assert_eq!(folder_rel(&link, &real, false).as_deref(), Some(""));
		assert_eq!(
			folder_rel(&real, &link.join("sub"), false).as_deref(),
			Some("sub")
		);
		// Outside still reads as outside after resolving.
		assert_eq!(folder_rel(&link, tmp.path(), false), None);
	}
}
