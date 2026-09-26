//! Shared visual tokens for the native workbench (IntelliJ New UI dark, compact).
//!
//! Every panel takes its colors and sizes from here so the palette is not
//! scattered across the render code.

// Sizes in logical pixels.
pub const HEADER_H: f32 = 38.0;
pub const STATUS_H: f32 = 22.0;
pub const RAIL_W: f32 = 36.0;
pub const ROW_H: f32 = 24.0;
pub const PANEL_HEADER_H: f32 = 28.0;
pub const SPLITTER: f32 = 4.0;
pub const UI_TEXT: f32 = 13.0;
pub const SMALL_TEXT: f32 = 12.0;
pub const CODE_TEXT: f32 = 12.5;
// Kept as an alias; "monospace" does not resolve to any family under GPUI on Linux.
pub const CODE_FONT: &str = EDITOR_FONT;

pub const LEFT_W_DEFAULT: f32 = 280.0;
pub const LEFT_W_MIN: f32 = 160.0;
pub const BOTTOM_H_DEFAULT: f32 = 230.0;
pub const BOTTOM_H_MIN: f32 = 100.0;

// Neutral surfaces.
pub const EDITOR_BG: u32 = 0x1e1f22;
pub const PANEL_BG: u32 = 0x2b2d30;
pub const HEADER_BG: u32 = 0x2b2d30;
pub const BORDER: u32 = 0x1e1f22;
pub const DIVIDER: u32 = 0x393b40;
pub const HOVER_BG: u32 = 0x393b40;
pub const SELECTION_BG: u32 = 0x2e436e;
pub const SELECTION_INACTIVE_BG: u32 = 0x43454a;
pub const RANGE_BG: u32 = 0x263150;
pub const RAIL_ACTIVE_BG: u32 = 0x43454a;
pub const TOOLTIP_BG: u32 = 0x3c3f41;

// Text.
pub const TEXT: u32 = 0xdfe1e5;
pub const TEXT_MUTED: u32 = 0x868a91;
pub const TEXT_DISABLED: u32 = 0x5a5d63;
pub const LINE_NUMBER: u32 = 0x4b5059;

// Controls.
pub const ACCENT: u32 = 0x3574f0;
pub const ACCENT_TEXT: u32 = 0xffffff;
pub const BUTTON_BG: u32 = 0x2b2d30;
pub const BUTTON_BORDER: u32 = 0x4e5157;
pub const CHECK_BORDER: u32 = 0x6f737a;

// Git status.
pub const GIT_ADDED: u32 = 0x73bd79;
pub const GIT_MODIFIED: u32 = 0x70aeff;
pub const GIT_DELETED: u32 = 0x8c8c8c;
pub const GIT_UNTRACKED: u32 = 0xd5756c;
pub const GIT_CONFLICT: u32 = 0xe0625b;
pub const WARNING: u32 = 0xf2c55c;
pub const ERROR: u32 = 0xf75464;
pub const ERROR_BG: u32 = 0x402929;

// Ref labels in the log.
pub const REF_HEAD: u32 = 0xdfe1e5;
pub const REF_LOCAL: u32 = 0x73bd79;
pub const REF_REMOTE: u32 = 0xb189f5;
pub const REF_TAG: u32 = 0xe5c07b;
pub const REF_BG: u32 = 0x393b40;

// Reader.
pub const FIND_BG: u32 = 0x3b514d;
pub const FIND_CURRENT_BG: u32 = 0x6b5b2a;
pub const CURRENT_LINE_BG: u32 = 0x26282e;
pub const DIFF_ADD_BG: u32 = 0x1f3325;
pub const DIFF_DEL_BG: u32 = 0x3d2328;
pub const DIFF_HUNK_BG: u32 = 0x252a37;
pub const DIFF_EMPTY_BG: u32 = 0x25262a;

// Keyboard focus ring (visible only on the focused control).
pub const FOCUS_RING: u32 = 0x3574f0;

// Icons.
pub const FOLDER: u32 = 0x87939a;
pub const FILE: u32 = 0x9aa0a8;

// Editor empty state: shortcut text in the hint list.
pub const HINT_SHORTCUT: u32 = 0x548af7;

// Code font for the reader. The generic "monospace" is not a family name
// GPUI can resolve on Linux (it falls back to a proportional UI font), so
// name a concrete monospace family per platform.
pub const EDITOR_FONT: &str = if cfg!(target_os = "macos") {
	"Menlo"
} else if cfg!(target_os = "windows") {
	"Consolas"
} else {
	"DejaVu Sans Mono"
};
