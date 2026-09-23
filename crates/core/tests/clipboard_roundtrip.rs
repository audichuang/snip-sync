//! Layer 4 of plan.md section 5: write the real system clipboard and read it
//! back. Skipped, with the reason printed, when there is no display (Linux
//! without X11 / Wayland; CI runs it under xvfb).

use snip_core::clip;

fn no_display() -> Option<String> {
	if cfg!(target_os = "linux")
		&& std::env::var_os("DISPLAY").is_none()
		&& std::env::var_os("WAYLAND_DISPLAY").is_none()
	{
		return Some("neither DISPLAY nor WAYLAND_DISPLAY is set".into());
	}
	arboard::Clipboard::new()
		.err()
		.map(|e| format!("no clipboard available: {e}"))
}

// One test: the clipboard is global, so the cases must not run in parallel.
#[test]
fn clipboard_round_trips_text() {
	if let Some(why) = no_display() {
		eprintln!("SKIPPED clipboard round trip: {why}");
		return;
	}
	let saved = clip::read_text().ok();

	let unicode = "繁體中文 ✓ emoji 🦀 combining e\u{301} RTL \u{5d0}\u{5d1} nul-free \u{3000}\u{feff}end";
	let crlf = "// file: a.txt\r\nline one\r\nline two\r\n\r\n";
	let mixed = "lf\ncrlf\r\nlone cr\rend\n";
	let line = "0123456789abcdef漢字\n";
	let large = line.repeat((1 << 20) / line.len() + 1);
	assert!(large.len() > 1 << 20);

	for (name, text) in [
		("unicode", unicode),
		("crlf", crlf),
		("mixed newlines", mixed),
		("1 MB", large.as_str()),
	] {
		clip::write_text(text).unwrap();
		let back = clip::read_text().unwrap();
		assert!(
			back == text,
			"{name}: {} bytes in, {} bytes out",
			text.len(),
			back.len()
		);
	}

	if let Some(saved) = saved {
		let _ = clip::write_text(&saved);
	}
}
