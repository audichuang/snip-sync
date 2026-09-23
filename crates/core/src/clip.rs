//! System clipboard access via arboard.

use std::sync::{Mutex, PoisonError};

/// First line of a commit-mode payload (spec 4.4). `commits` refers to this constant.
pub const COMMIT_MARKER: &str = "// snip-sync commits v1";

pub type Result<T> = std::result::Result<T, arboard::Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	Commits,
	Files,
}

/// Commit mode iff the first line is the marker (tolerating CRLF from Windows clipboards).
pub fn detect_mode(text: &str) -> Mode {
	let first = text.split('\n').next().unwrap_or("");
	if first.trim_end_matches('\r') == COMMIT_MARKER {
		Mode::Commits
	} else {
		Mode::Files
	}
}

// On Linux the clipboard contents are hosted by the process through a live `Clipboard`
// instance; dropping the last one makes them vanish. Keep one for the process lifetime
// so `write_text` sticks in a resident process (the desktop app). It is never dropped,
// so contents disappear when the process exits; short-lived callers use
// `write_text_and_wait`. The mutex also serializes access, as arboard advises on Windows.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn with_clipboard<T>(
	f: impl FnOnce(&mut arboard::Clipboard) -> Result<T>,
) -> Result<T> {
	let mut guard = CLIPBOARD.lock().unwrap_or_else(PoisonError::into_inner);
	if guard.is_none() {
		*guard = Some(arboard::Clipboard::new()?);
	}
	f(guard.as_mut().expect("initialized above"))
}

pub fn read_text() -> Result<String> {
	with_clipboard(|c| c.get_text())
}

/// Suitable for long-running processes (the desktop app) that keep clipboard ownership.
pub fn write_text(text: &str) -> Result<()> {
	with_clipboard(|c| c.set_text(text))
}

/// For short-lived processes (the CLI): on Linux, blocks until another program takes
/// the clipboard contents, since they vanish when the owning process exits.
/// Elsewhere it is the same as `write_text`.
pub fn write_text_and_wait(text: &str) -> Result<()> {
	#[cfg(target_os = "linux")]
	{
		use arboard::SetExtLinux;
		arboard::Clipboard::new()?.set().wait().text(text)
	}
	#[cfg(not(target_os = "linux"))]
	{
		write_text(text)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn detect_mode_commits_marker() {
		assert_eq!(detect_mode("// snip-sync commits v1\n{}"), Mode::Commits);
		assert_eq!(detect_mode("// snip-sync commits v1\r\n{}"), Mode::Commits);
		assert_eq!(detect_mode(COMMIT_MARKER), Mode::Commits);
	}

	#[test]
	fn detect_mode_files() {
		assert_eq!(detect_mode(""), Mode::Files);
		assert_eq!(detect_mode("// src/a.rs\nfn main() {}"), Mode::Files);
		// Marker must be the whole first line, not a prefix or a later line.
		assert_eq!(detect_mode("// snip-sync commits v12\n{}"), Mode::Files);
		assert_eq!(detect_mode("x\n// snip-sync commits v1\n{}"), Mode::Files);
		assert_eq!(detect_mode(" // snip-sync commits v1\n{}"), Mode::Files);
	}

	fn has_display() -> bool {
		// An empty variable counts as unset.
		let unset = |k: &str| std::env::var_os(k).is_none_or(|v| v.is_empty());
		if cfg!(target_os = "linux")
			&& unset("DISPLAY")
			&& unset("WAYLAND_DISPLAY")
		{
			eprintln!("skipping clipboard test: no DISPLAY or WAYLAND_DISPLAY");
			return false;
		}
		true
	}

	#[test]
	fn clip_round_trip() {
		if !has_display() {
			return;
		}
		let text = "snip clip test\n中文\r\nend";
		write_text(text).unwrap();
		assert_eq!(read_text().unwrap(), text);
	}
}
