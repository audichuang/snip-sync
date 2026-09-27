//! IntelliJ New UI ("expui") icons, vendored as SVG under
//! `assets/icons/expui/` (Apache-2.0, see the README there) and embedded
//! into the binary. Every icon has a light and a `_dark` file; the active
//! palette picks one.
//!
//! [`icon`] paints the SVG in its own colours, like IntelliJ does for almost
//! every icon. [`icon_tinted`] paints it as a one-colour mask, for the few
//! places IntelliJ recolours an icon (white on an accent fill).

use std::borrow::Cow;

use gpui::{
	img, prelude::*, px, rgb, svg, AssetSource, Img, SharedString, Svg,
};

macro_rules! icons {
	($($variant:ident => $stem:literal,)*) => {
		/// One IntelliJ icon. Several variants may share a file where the
		/// New UI set has no distinct glyph (e.g. `FolderOpen`).
		#[derive(Clone, Copy, Debug, PartialEq, Eq)]
		#[allow(dead_code)]
		pub enum Icon {
			$($variant,)*
		}

		impl Icon {
			pub const ALL: &'static [Icon] = &[$(Icon::$variant,)*];

			/// Asset path of the light or dark file.
			pub fn path(self, dark: bool) -> &'static str {
				match (self, dark) {
					$(
						(Icon::$variant, false) => concat!("icons/expui/", $stem, ".svg"),
						(Icon::$variant, true) => concat!("icons/expui/", $stem, "_dark.svg"),
					)*
				}
			}

			fn bytes(self, dark: bool) -> &'static [u8] {
				match (self, dark) {
					$(
						(Icon::$variant, false) => include_bytes!(concat!("../assets/icons/expui/", $stem, ".svg")),
						(Icon::$variant, true) => include_bytes!(concat!("../assets/icons/expui/", $stem, "_dark.svg")),
					)*
				}
			}
		}
	};
}

icons! {
	Refresh => "general/refresh",
	Locate => "general/locate",
	Diff => "vcs/diff",
	Copy => "general/copy",
	Paste => "general/paste",
	Basket => "vcs/shelve",
	Eye => "general/show",
	Hide => "general/hide",
	More => "general/moreVertical",
	Search => "general/search",
	Regex => "inline/regex",
	MatchCase => "inline/matchCase",
	Close => "general/close",
	ChevronDown => "general/chevronDown",
	ChevronRight => "general/chevronRight",
	Branch => "general/vcs",
	RemoteBranch => "toolwindows/web",
	Tag => "dvcs/branchLabel",
	Head => "dvcs/currentBranchLabel",
	Commit => "vcs/commit",
	Folder => "nodes/folder",
	FolderOpen => "nodes/folder",
	File => "fileTypes/anyType",
	FileRust => "language/rust",
	FileTs => "fileTypes/typeScript",
	FileJs => "fileTypes/javaScript",
	FileJson => "fileTypes/json",
	FileMarkdown => "fileTypes/markdown",
	FileText => "fileTypes/text",
	FileImage => "fileTypes/image",
	FileToml => "fileTypes/toml",
	FileYaml => "fileTypes/yaml",
	Filter => "general/filter",
	User => "general/user",
	Calendar => "general/history",
	Paths => "general/listFiles",
	ArrowUp => "general/up",
	ArrowDown => "general/down",
	SideBySide => "diff/sideBySide",
	Unified => "diff/unified",
	GoToLine => "general/hashtag",
	NextDiff => "general/down",
	PrevDiff => "general/up",
	Checked => "actions/checked",
	Settings => "general/settings",
	Project => "toolwindows/project",
	Changes => "toolwindows/changes",
	GitLog => "toolwindows/vcs",
	Language => "general/language",
	Warning => "status/warning",
	Error => "status/error",
	Info => "status/info",
	Plus => "general/add",
	Minus => "general/remove",
	ExpandAll => "general/expandAll",
	CollapseAll => "general/collapseAll",
	Pin => "general/pin",
	Cancel => "vcs/abort",
	Apply => "general/greenCheckmark",
	Back => "general/left",
	SelectAll => "actions/selectAll",
	SelectNone => "actions/unselectAll",
}

fn dark() -> bool {
	!crate::theme::is_light()
}

/// A `size`×`size` icon in IntelliJ's own colours for the active theme.
pub fn icon(kind: Icon, size: f32) -> Img {
	img(kind.path(dark())).flex_shrink_0().size(px(size))
}

/// A `size`×`size` icon painted entirely in `color` (IntelliJ's selected /
/// accent-fill recolouring).
pub fn icon_tinted(kind: Icon, size: f32, color: u32) -> Svg {
	svg()
		.path(kind.path(dark()))
		.flex_shrink_0()
		.size(px(size))
		.text_color(rgb(color))
}

/// The icon IntelliJ shows for a file, chosen by name/extension.
pub fn file_icon(path: &str) -> Icon {
	let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
	let ext = name
		.rsplit_once('.')
		.map(|(_, e)| e.to_ascii_lowercase())
		.unwrap_or_default();
	match ext.as_str() {
		"rs" => Icon::FileRust,
		"ts" | "tsx" | "mts" | "cts" => Icon::FileTs,
		"js" | "jsx" | "mjs" | "cjs" => Icon::FileJs,
		"json" | "jsonc" | "json5" => Icon::FileJson,
		"md" | "markdown" => Icon::FileMarkdown,
		"txt" | "log" => Icon::FileText,
		"png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico"
		| "tif" | "tiff" => Icon::FileImage,
		"toml" => Icon::FileToml,
		"yaml" | "yml" => Icon::FileYaml,
		_ => Icon::File,
	}
}

/// Serves the embedded icons to GPUI (`img`/`svg` load through this).
pub struct Assets;

impl AssetSource for Assets {
	fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
		Ok(lookup(path).map(Cow::Borrowed))
	}

	fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
		Ok(Vec::new())
	}
}

fn lookup(path: &str) -> Option<&'static [u8]> {
	Icon::ALL.iter().find_map(|&i| {
		[false, true]
			.into_iter()
			.find(|&d| i.path(d) == path)
			.map(|d| i.bytes(d))
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_icon_resolves_for_both_themes() {
		for &i in Icon::ALL {
			for d in [false, true] {
				let bytes = Assets.load(i.path(d)).unwrap().unwrap();
				let text = std::str::from_utf8(&bytes).unwrap();
				assert!(text.contains("<svg"), "{i:?} dark={d}");
			}
			assert_ne!(i.bytes(false), i.bytes(true), "{i:?} light == dark");
		}
		assert!(Assets.load("icons/expui/nope.svg").unwrap().is_none());
	}

	#[test]
	fn file_icon_by_extension() {
		assert_eq!(file_icon("src/main.rs"), Icon::FileRust);
		assert_eq!(file_icon("a/b.TSX"), Icon::FileTs);
		assert_eq!(file_icon("pkg.json"), Icon::FileJson);
		assert_eq!(file_icon("README.md"), Icon::FileMarkdown);
		assert_eq!(file_icon("Cargo.toml"), Icon::FileToml);
		assert_eq!(file_icon(".github\\ci.yml"), Icon::FileYaml);
		assert_eq!(file_icon("logo.PNG"), Icon::FileImage);
		assert_eq!(file_icon("Makefile"), Icon::File);
		assert_eq!(file_icon("dir.d/noext"), Icon::File);
	}
}
