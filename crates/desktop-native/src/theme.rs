//! Shared visual tokens for the native workbench: IntelliJ New UI "Islands"
//! Dark and Light, compact density.
//!
//! Every panel takes its colors and sizes from here. Colors live in a
//! [`Palette`]; render code reads the active one through [`pal()`], so
//! switching palette and re-rendering recolours the whole window.
//!
//! Sources (JetBrains/intellij-community, master, fetched 2026-09-28):
//! - UI: `platform/platform-resources/src/themes/islands/ManyIslandsDark.theme.json`
//!   and `ManyIslandsLight.theme.json` (semantic `colors` map, `Island.*`,
//!   `VersionControl.GitLog.*`, `VersionControl.Log.Graph.*`).
//! - Editor: `themes/islands/IslandSchemeDark.xml`, `themes/expUI/expUI_darkScheme.xml`
//!   and `themes/expUI/expUI_lightScheme.xml` (TEXT, FILESTATUS_*, CARET_ROW,
//!   LINE_NUMBERS, search results, DIFF_SEPARATORS, syntax foregrounds).
//! - Diff line backgrounds are inherited by those schemes from the bundled
//!   Darcula / Default schemes (DIFF_INSERTED/DELETED/MODIFIED), which are
//!   not in that directory; the values below are those schemes' well-known
//!   defaults.
//!
//! Translucent IntelliJ tokens (hover `#FFFFFF17` / `#00000012`) are
//! pre-composited over the island background because the render code paints
//! opaque `rgb()` colors.

use std::sync::atomic::{AtomicBool, Ordering};

// Sizes in logical pixels.
pub const HEADER_H: f32 = 38.0;
pub const STATUS_H: f32 = 22.0;
pub const RAIL_W: f32 = 36.0;
pub const ROW_H: f32 = 24.0;
pub const PANEL_HEADER_H: f32 = 28.0;
/// Splitters double as the frame gap between islands (`Island.borderWidth.compact`).
pub const SPLITTER: f32 = 4.0;
/// Corner radius of an island (`Island.arc.compact` is 16, i.e. an 8px radius).
pub const ISLAND_RADIUS: f32 = 8.0;
pub const UI_TEXT: f32 = 13.0;
pub const SMALL_TEXT: f32 = 12.0;
pub const CODE_TEXT: f32 = 13.0;

pub const LEFT_W_DEFAULT: f32 = 280.0;
pub const LEFT_W_MIN: f32 = 160.0;
pub const BOTTOM_H_DEFAULT: f32 = 230.0;
pub const BOTTOM_H_MIN: f32 = 100.0;

/// UI font, embedded and registered at startup (see [`register_fonts`]).
pub const UI_FONT: &str = "Inter";
/// Code font for the reader, embedded like [`UI_FONT`].
pub const EDITOR_FONT: &str = "JetBrains Mono";
pub const CODE_FONT: &str = EDITOR_FONT;

/// Every color the native UI paints, as `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
	// Surfaces.
	/// Frame behind the islands (`main-window-bg`), also the header/status bar.
	pub frame_bg: u32,
	pub editor_bg: u32,
	/// Tool window island (`tool-window-bg`).
	pub panel_bg: u32,
	pub header_bg: u32,
	pub border: u32,
	pub divider: u32,
	pub hover_bg: u32,
	pub selection_bg: u32,
	pub selection_inactive_bg: u32,
	pub range_bg: u32,
	/// Active tool-window stripe button (`ToolWindow.Button.selectedBackground`).
	pub rail_active_bg: u32,
	pub tooltip_bg: u32,
	// Text.
	pub text: u32,
	pub text_muted: u32,
	pub text_disabled: u32,
	pub line_number: u32,
	pub link: u32,
	// Controls.
	pub accent: u32,
	pub accent_text: u32,
	pub button_bg: u32,
	pub button_border: u32,
	pub check_border: u32,
	pub focus_ring: u32,
	// File status.
	pub git_added: u32,
	pub git_modified: u32,
	pub git_deleted: u32,
	pub git_untracked: u32,
	pub git_conflict: u32,
	// Feedback.
	pub warning: u32,
	pub warning_bg: u32,
	pub error: u32,
	pub error_bg: u32,
	// Log ref labels.
	pub ref_head: u32,
	pub ref_local: u32,
	pub ref_remote: u32,
	pub ref_tag: u32,
	pub ref_bg: u32,
	/// Log graph lanes, indexed modulo length by the core lane color index.
	pub graph_lanes: [u32; 12],
	// Reader.
	pub find_bg: u32,
	pub find_current_bg: u32,
	pub current_line_bg: u32,
	pub diff_add_bg: u32,
	pub diff_del_bg: u32,
	pub diff_mod_bg: u32,
	pub diff_hunk_bg: u32,
	pub diff_empty_bg: u32,
	// Syntax.
	pub code_text: u32,
	pub syntax_keyword: u32,
	pub syntax_string: u32,
	pub syntax_number: u32,
	pub syntax_comment: u32,
	pub syntax_type: u32,
	pub diff_hunk_text: u32,
	// Icons.
	pub folder: u32,
	pub file: u32,
}

/// Islands Dark.
pub static DARK: Palette = Palette {
	frame_bg: 0x26282c,              // main-window-bg
	editor_bg: 0x191a1c,             // editor-bg
	panel_bg: 0x191a1c,              // tool-window-bg
	header_bg: 0x26282c,             // MainToolbar.background = main-window-bg
	border: 0x26282c,                // tool-window-border
	divider: 0x33353b,               // Separator.separatorColor = popup-border
	hover_bg: 0x2e2f30,              // toolbar-bg-hovered #FFFFFF17 over #191A1C
	selection_bg: 0x2a4371,          // selection-bg-active
	selection_inactive_bg: 0x33353b, // selection-bg-inactive
	range_bg: 0x233558,              // selection-bg-active-muted
	rail_active_bg: 0x3871e1,        // toolbar-selected-bg-active
	tooltip_bg: 0x33353b,            // ToolTip.background = feedback-bg
	text: 0xd1d3d9,                  // text-default
	text_muted: 0x9fa2a8,            // text-muted
	text_disabled: 0x4c4f56,         // text-disabled
	line_number: 0x4b5059,           // LINE_NUMBERS_COLOR
	link: 0x71a1fe,                  // text-link
	accent: 0x3871e1,                // control-brand-bg
	accent_text: 0xffffff,           // text-over-accent
	button_bg: 0x191a1c,             // control-bg
	button_border: 0x40434a,         // control-border
	check_border: 0x73767c,          // text-secondary
	focus_ring: 0x3871e1,            // control-brand-border
	git_added: 0x73bd79,             // FILESTATUS_ADDED
	git_modified: 0x70aeff,          // FILESTATUS_MODIFIED
	git_deleted: 0x6f737a,           // FILESTATUS_DELETED
	git_untracked: 0xe88f89,         // FILESTATUS_UNKNOWN
	git_conflict: 0xde6a66,          // FILESTATUS_..._MERGED_WITH_CONFLICTS
	warning: 0xd59637,               // text-warning
	warning_bg: 0x44321d,            // feedback-warning-bg
	error: 0xf57e84,                 // text-error
	error_bg: 0x56272b,              // feedback-error-bg
	ref_head: 0xf5d273,              // VersionControl.GitLog.headIconColor
	ref_local: 0x5fad65,             // .localBranchIconColor
	ref_remote: 0xb589ec,            // .remoteBranchIconColor
	ref_tag: 0x868a91,               // .tagIconColor
	ref_bg: 0x26282c,                // layer-1-bg
	// Core palette hues at VersionControl.Log.Graph saturation 0.6 / brightness 0.6.
	graph_lanes: [
		0x3d6e99, 0x5f993d, 0x99573d, 0x693d99, 0x993d6d, 0x3d9995, 0x997e3d,
		0x993d3e, 0x3d5399, 0x663d99, 0x3d9990, 0x997a3d,
	],
	find_bg: 0x114957,         // TEXT_SEARCH_RESULT_ATTRIBUTES
	find_current_bg: 0x2d543f, // SEARCH_RESULT_ATTRIBUTES
	current_line_bg: 0x1f2024, // CARET_ROW_COLOR
	diff_add_bg: 0x294436,     // Darcula DIFF_INSERTED
	diff_del_bg: 0x484a4a,     // Darcula DIFF_DELETED
	diff_mod_bg: 0x385570,     // Darcula DIFF_MODIFIED
	diff_hunk_bg: 0x2b2d30,    // DIFF_SEPARATORS_BACKGROUND
	diff_empty_bg: 0x212326,   // editor-bg-inline
	code_text: 0xbcbec4,       // TEXT
	syntax_keyword: 0xcf8e6d,  // DEFAULT_KEYWORD
	syntax_string: 0x6aab73,   // DEFAULT_STRING
	syntax_number: 0x2aacb8,   // DEFAULT_NUMBER
	syntax_comment: 0x7a7e85,  // DEFAULT_LINE_COMMENT
	syntax_type: 0x16baac,     // Darcula class name (teal)
	diff_hunk_text: 0x6f737a,  // WHITESPACES
	folder: 0xced0d6,          // New UI dark icon ink
	file: 0x9fa2a8,            // text-muted
};

/// Islands Light.
pub static LIGHT: Palette = Palette {
	frame_bg: 0xe9eaee,              // main-window-bg
	editor_bg: 0xffffff,             // editor-bg
	panel_bg: 0xffffff,              // tool-window-bg
	header_bg: 0xe9eaee,             // main-window-bg
	border: 0xe9eaee,                // tool-window-border
	divider: 0xdddfe4,               // layer-1-border
	hover_bg: 0xededed,              // toolbar-bg-hovered #00000012 over #FFFFFF
	selection_bg: 0xd0dffe,          // selection-bg-active
	selection_inactive_bg: 0xe9eaee, // selection-bg-inactive
	range_bg: 0xe3ebfe,              // selection-bg-active-muted
	rail_active_bg: 0x3871e1,        // toolbar-selected-bg-active
	tooltip_bg: 0xf7f8f9,            // popup-bg-inline
	text: 0x000000,                  // text-default
	text_muted: 0x5f6269,            // text-muted
	text_disabled: 0x9fa2a8,         // text-disabled
	line_number: 0xaeb3c2,           // LINE_NUMBERS_COLOR
	link: 0x2f5eb9,                  // text-link
	accent: 0x3871e1,                // control-brand-bg
	accent_text: 0xffffff,           // text-over-accent
	button_bg: 0xffffff,             // control-bg
	button_border: 0xd1d3d9,         // control-border
	check_border: 0x8b8e94,          // gray-90
	focus_ring: 0x3871e1,            // control-brand-border
	git_added: 0x067d17,             // FILESTATUS_ADDED
	git_modified: 0x0033b3,          // FILESTATUS_MODIFIED
	git_deleted: 0x6c707e,           // FILESTATUS_DELETED
	git_untracked: 0xb23247,         // FILESTATUS_UNKNOWN
	git_conflict: 0xde1b2e,          // FILESTATUS_..._MERGED_WITH_CONFLICTS
	warning: 0xa56906,               // text-warning
	warning_bg: 0xfff6e9,            // feedback-warning-bg
	error: 0xc54e58,                 // text-error
	error_bg: 0xfff6f5,              // feedback-error-bg
	ref_head: 0xffaf0f,              // VersionControl.GitLog.headIconColor
	ref_local: 0x369650,             // .localBranchIconColor
	ref_remote: 0x834df0,            // .remoteBranchIconColor
	ref_tag: 0x6c707e,               // .tagIconColor
	ref_bg: 0xf7f8f9,                // layer-1-bg
	// Core palette hues at VersionControl.Log.Graph saturation 0.6 / brightness 0.7.
	graph_lanes: [
		0x4780b2, 0x6eb247, 0xb26647, 0x7b47b2, 0xb2477f, 0x47b2ae, 0xb29247,
		0xb24749, 0x4760b2, 0x7847b2, 0x47b2a8, 0xb28f47,
	],
	find_bg: 0xfcd47e,         // TEXT_SEARCH_RESULT_ATTRIBUTES
	find_current_bg: 0xccccff, // Default SEARCH_RESULT_ATTRIBUTES
	current_line_bg: 0xf5f8fe, // CARET_ROW_COLOR
	diff_add_bg: 0xbaeeba,     // Default DIFF_INSERTED
	diff_del_bg: 0xd6d6d6,     // Default DIFF_DELETED
	diff_mod_bg: 0xc2d8f2,     // DIFF_MODIFIED
	diff_hunk_bg: 0xe4e6eb,    // DIFF_SEPARATORS_BACKGROUND
	diff_empty_bg: 0xf7f8f9,   // editor-bg-inline
	code_text: 0x080808,       // TEXT
	syntax_keyword: 0x0033b3,  // DEFAULT_KEYWORD
	syntax_string: 0x067d17,   // DEFAULT_STRING
	syntax_number: 0x1750eb,   // DEFAULT_NUMBER
	syntax_comment: 0x8c8c8c,  // DEFAULT_LINE_COMMENT
	syntax_type: 0x008080,     // Default class-level teal
	diff_hunk_text: 0x8c8c8c,  // DEFAULT_LINE_COMMENT
	folder: 0x6c707e,          // New UI light icon ink
	file: 0x818594,            // unmatchedForeground
};

static LIGHT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The palette the next frame paints with.
pub fn pal() -> &'static Palette {
	if LIGHT_ACTIVE.load(Ordering::Relaxed) {
		&LIGHT
	} else {
		&DARK
	}
}

/// Whether the light palette is active (icons pick their light files).
pub fn is_light() -> bool {
	LIGHT_ACTIVE.load(Ordering::Relaxed)
}

/// `SNIP_THEME=dark|light` pins the palette; anything else follows the OS.
fn env_override() -> Option<bool> {
	match std::env::var("SNIP_THEME")
		.ok()?
		.to_ascii_lowercase()
		.as_str()
	{
		"light" => Some(true),
		"dark" => Some(false),
		_ => None,
	}
}

/// Selects the palette for `appearance` (or the `SNIP_THEME` override).
/// Returns whether the active palette changed.
pub fn sync_appearance(appearance: gpui::WindowAppearance) -> bool {
	use gpui::WindowAppearance::*;
	let light =
		env_override().unwrap_or(matches!(appearance, Light | VibrantLight));
	LIGHT_ACTIVE.swap(light, Ordering::Relaxed) != light
}

/// Embedded fonts (SIL OFL 1.1; licenses next to the files).
pub fn register_fonts(cx: &gpui::App) {
	let fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
		include_bytes!("../assets/fonts/Inter-Regular.ttf")
			.as_slice()
			.into(),
		include_bytes!("../assets/fonts/Inter-SemiBold.ttf")
			.as_slice()
			.into(),
		include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")
			.as_slice()
			.into(),
	];
	if let Err(e) = cx.text_system().add_fonts(fonts) {
		eprintln!("Failed to register embedded fonts: {e:?}");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn light_and_dark_differ_except_brand_tokens() {
		// Brand blue and text on it are shared by Islands Dark and Light.
		let shared = [
			(DARK.accent, LIGHT.accent),
			(DARK.accent_text, LIGHT.accent_text),
			(DARK.rail_active_bg, LIGHT.rail_active_bg),
			(DARK.focus_ring, LIGHT.focus_ring),
		];
		for (d, l) in shared {
			assert_eq!(d, l);
		}
		let (d, l) = (&DARK, &LIGHT);
		let pairs = [
			("frame_bg", d.frame_bg, l.frame_bg),
			("editor_bg", d.editor_bg, l.editor_bg),
			("panel_bg", d.panel_bg, l.panel_bg),
			("header_bg", d.header_bg, l.header_bg),
			("border", d.border, l.border),
			("divider", d.divider, l.divider),
			("hover_bg", d.hover_bg, l.hover_bg),
			("selection_bg", d.selection_bg, l.selection_bg),
			(
				"selection_inactive_bg",
				d.selection_inactive_bg,
				l.selection_inactive_bg,
			),
			("range_bg", d.range_bg, l.range_bg),
			("tooltip_bg", d.tooltip_bg, l.tooltip_bg),
			("text", d.text, l.text),
			("text_muted", d.text_muted, l.text_muted),
			("text_disabled", d.text_disabled, l.text_disabled),
			("line_number", d.line_number, l.line_number),
			("link", d.link, l.link),
			("button_bg", d.button_bg, l.button_bg),
			("button_border", d.button_border, l.button_border),
			("check_border", d.check_border, l.check_border),
			("git_added", d.git_added, l.git_added),
			("git_modified", d.git_modified, l.git_modified),
			("git_deleted", d.git_deleted, l.git_deleted),
			("git_untracked", d.git_untracked, l.git_untracked),
			("git_conflict", d.git_conflict, l.git_conflict),
			("warning", d.warning, l.warning),
			("warning_bg", d.warning_bg, l.warning_bg),
			("error", d.error, l.error),
			("error_bg", d.error_bg, l.error_bg),
			("ref_head", d.ref_head, l.ref_head),
			("ref_local", d.ref_local, l.ref_local),
			("ref_remote", d.ref_remote, l.ref_remote),
			("ref_tag", d.ref_tag, l.ref_tag),
			("ref_bg", d.ref_bg, l.ref_bg),
			("find_bg", d.find_bg, l.find_bg),
			("find_current_bg", d.find_current_bg, l.find_current_bg),
			("current_line_bg", d.current_line_bg, l.current_line_bg),
			("diff_add_bg", d.diff_add_bg, l.diff_add_bg),
			("diff_del_bg", d.diff_del_bg, l.diff_del_bg),
			("diff_mod_bg", d.diff_mod_bg, l.diff_mod_bg),
			("diff_hunk_bg", d.diff_hunk_bg, l.diff_hunk_bg),
			("diff_empty_bg", d.diff_empty_bg, l.diff_empty_bg),
			("code_text", d.code_text, l.code_text),
			("syntax_keyword", d.syntax_keyword, l.syntax_keyword),
			("syntax_string", d.syntax_string, l.syntax_string),
			("syntax_number", d.syntax_number, l.syntax_number),
			("syntax_comment", d.syntax_comment, l.syntax_comment),
			("syntax_type", d.syntax_type, l.syntax_type),
			("diff_hunk_text", d.diff_hunk_text, l.diff_hunk_text),
			("folder", d.folder, l.folder),
			("file", d.file, l.file),
		];
		for (name, dark, light) in pairs {
			assert_ne!(
				dark, light,
				"{name} must differ between DARK and LIGHT"
			);
		}
		for (i, (dark, light)) in
			d.graph_lanes.iter().zip(&l.graph_lanes).enumerate()
		{
			assert_ne!(dark, light, "graph lane {i}");
		}
		// Light surfaces are light and dark surfaces are dark.
		let luma = |c: u32| {
			let [_, r, g, b] = c.to_be_bytes();
			(u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114)
				/ 1000
		};
		assert!(luma(d.editor_bg) < 64 && luma(l.editor_bg) > 192);
		assert!(luma(d.text) > 192 && luma(l.text) < 64);
	}

	#[test]
	fn graph_lanes_cover_core_palette() {
		assert_eq!(
			DARK.graph_lanes.len(),
			snip_core::graph::COLOR_PALETTE.len()
		);
	}
}
