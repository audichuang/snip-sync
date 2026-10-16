//! The folder a paste preview writes into, as the user picked it (#238).
//!
//! The session keeps it apart from the plan: a plan is rebuilt, disarmed
//! and refused over its life, while the folder is what the user chose and
//! must survive all of that.
//!
//! A folder is named in one of two ways, and they are two types of one
//! argument: a folder the file system names (a tree row, a breadcrumb, the
//! dialog, the Project menu) is data, and [`WsRel::of_folder`] keeps every
//! character of it; only what the user typed is text, and only
//! [`WsRel::parse_typed`] trims it. A name may begin or end with a space,
//! or be nothing else.

use std::path::{Component, Path, PathBuf};

use crate::i18n::Msg;

/// The destination folder of the open paste preview: a tree path (a local
/// path, or a path under the remote session root) and where it lies in the
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestFolder {
	pub path: PathBuf,
	/// Where it lies in the workspace. `None` is a folder outside the
	/// workspace (an enclosing repo, a `--restore-dir` elsewhere), shown as
	/// one plain label.
	pub rel: Option<WsRel>,
}

impl DestFolder {
	pub fn new(
		workspace_root: &Path,
		path: PathBuf,
		remote: bool,
	) -> DestFolder {
		let rel = WsRel::of_folder(workspace_root, &path, remote);
		DestFolder { path, rel }
	}
}

/// A folder of the workspace by its place in it: the names from the
/// workspace down, "/"-separated, each name exactly as the file system has
/// it. "" is the workspace itself; no name is empty, `.` or `..`.
///
/// There is no way to make one from a bare string except
/// [`WsRel::parse_typed`], which is for what a person typed. Every other
/// folder is read off a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsRel(String);

impl WsRel {
	/// The workspace itself.
	pub fn root() -> WsRel {
		WsRel(String::new())
	}

	/// The place of the folder `path` in the workspace, for a folder the
	/// file system named (a tree row, the dialog, a repo): its names are
	/// data and none is changed, trimmed or skipped. `None` when it lies
	/// outside the workspace or a name is not UTF-8 text.
	pub fn of_folder(
		workspace_root: &Path,
		path: &Path,
		remote: bool,
	) -> Option<WsRel> {
		folder_rel(workspace_root, path, remote).map(WsRel)
	}

	/// The place a person typed: text, so the spaces around it are not part
	/// of it (a name that begins or ends with a space is reached by the
	/// field's own untouched text, or by a gesture). A path outside the
	/// workspace (absolute, or with `.`/`..`/empty parts) is `Err` with the
	/// text to show.
	pub fn parse_typed(typed: &str) -> Result<WsRel, String> {
		let typed = typed.trim();
		let text = if cfg!(windows) {
			typed.replace('\\', "/")
		} else {
			typed.to_string()
		};
		let absolute = Path::new(typed).is_absolute()
			|| text.starts_with('/')
			|| (cfg!(windows) && text.as_bytes().get(1) == Some(&b':'));
		if absolute {
			return Err(typed.to_string());
		}
		let rel = text.trim_end_matches('/');
		let rel = if rel == "." { "" } else { rel };
		if rel
			.split('/')
			.any(|part| !rel.is_empty() && matches!(part, "" | "." | ".."))
		{
			return Err(typed.to_string());
		}
		Ok(WsRel(rel.to_string()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}

	pub fn is_root(&self) -> bool {
		self.0.is_empty()
	}

	/// The names, from the workspace down.
	pub fn parts(&self) -> impl Iterator<Item = &str> {
		self.0.split('/').filter(|part| !part.is_empty())
	}

	/// Whether the last name ends with an ASCII space, tab or line break.
	/// The engine ([`snip_core`]'s root normalisation, the TS reference's
	/// `trim()`) trims the end of a destination root's path, so it writes
	/// into the parent, or refuses with "unsafe path"; a folder named so
	/// cannot be pasted into, and the bar says so before a plan is made.
	pub fn ends_with_space(&self) -> bool {
		self.parts().last().is_some_and(|name| {
			// The "x" shields the name's own leading spaces, which stay.
			let rooted = format!("x{name}");
			snip_core::format::ascii_trim(&rooted).len() != rooted.len()
		})
	}

	/// The folder holding the first `n` names: the breadcrumb's segment.
	pub fn ancestor(&self, n: usize) -> WsRel {
		WsRel(self.parts().take(n).collect::<Vec<_>>().join("/"))
	}

	/// The tree path of this folder under `workspace_root`.
	pub fn to_path(&self, workspace_root: &Path) -> PathBuf {
		self.parts()
			.fold(workspace_root.to_path_buf(), |path, part| path.join(part))
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
fn folder_rel(
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
	fn a_folder_the_file_system_names_keeps_every_character() {
		let ws = Path::new("/w/ws");
		for name in ["docs ", " docs", "   ", "a  b", "\u{3000}"] {
			let rel = WsRel::of_folder(ws, &ws.join(name), false).unwrap();
			assert_eq!(rel.as_str(), name);
			assert_eq!(rel.to_path(ws), ws.join(name));
			assert_eq!(rel.parts().collect::<Vec<_>>(), [name]);
		}
		let nested = WsRel::of_folder(ws, &ws.join("lead ").join("in"), false);
		let nested = nested.unwrap();
		assert_eq!(nested.as_str(), "lead /in");
		assert_eq!(nested.ancestor(1).as_str(), "lead ");
		assert_eq!(nested.ancestor(0), WsRel::root());
		assert_eq!(nested.ancestor(9), nested);
	}

	#[test]
	fn a_name_the_engine_would_trim_ends_with_space() {
		let ws = Path::new("/w");
		let rel =
			|name: &str| WsRel::of_folder(ws, &ws.join(name), false).unwrap();
		for name in ["docs ", "   ", "a\t", "a b\n"] {
			assert!(rel(name).ends_with_space(), "{name:?}");
		}
		// Spaces inside or in front stay, and so does a non-ASCII one.
		for name in [" docs", "a b", "a\u{a0}", "x"] {
			assert!(!rel(name).ends_with_space(), "{name:?}");
		}
		assert!(!rel("lead /in").ends_with_space());
		assert!(rel("in/lead ").ends_with_space());
		assert!(!WsRel::root().ends_with_space());
	}

	#[test]
	fn only_typed_text_is_trimmed() {
		let typed = |s: &str| WsRel::parse_typed(s).map(|r| r.as_str().into());
		assert_eq!(typed("  docs  "), Ok("docs".to_string()));
		assert_eq!(typed(" a / b "), Ok("a / b".to_string()));
		assert_eq!(typed("docs/"), Ok("docs".to_string()));
		assert_eq!(typed("."), Ok(String::new()));
		assert_eq!(typed("   "), Ok(String::new()));
		assert_eq!(typed("../x"), Err("../x".to_string()));
		assert_eq!(typed(" /abs "), Err("/abs".to_string()));
		assert_eq!(typed("a//b"), Err("a//b".to_string()));
	}

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
