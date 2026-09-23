//! Layer 4 of plan.md section 5: write the real system clipboard and read it
//! back. Skipped, with the reason printed, when there is no display (Linux
//! without X11 / Wayland; CI runs it under xvfb).
//!
//! The writer is a child process (this test binary re-spawned with
//! `CHILD_ENV` set) using `write_text_and_wait`, as the CLI does, and the
//! parent reads. A same-process read would be answered from arboard's own
//! in-memory copy and never exercise the X11 / Wayland selection transfer
//! (incl. INCR chunking for the 1 MB case).

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use snip_core::clip;

const CHILD_ENV: &str = "SNIP_CLIP_TEST_WRITER";
const TEST_NAME: &str = "clipboard_round_trips_text";

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

/// Kills the writer on drop, so a failed assertion does not leak it.
struct Writer(Child);

impl Drop for Writer {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Spawn a writer that owns the clipboard with `text` until it is overwritten.
fn spawn_writer(text: &str) -> Writer {
	let mut child = Command::new(std::env::current_exe().unwrap())
		.args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
		.env(CHILD_ENV, "1")
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();
	child
		.stdin
		.take()
		.unwrap()
		.write_all(text.as_bytes())
		.unwrap();
	Writer(child)
}

// One test: the clipboard is global, so the cases must not run in parallel.
#[test]
fn clipboard_round_trips_text() {
	if std::env::var_os(CHILD_ENV).is_some() {
		let mut text = String::new();
		std::io::stdin().read_to_string(&mut text).unwrap();
		clip::write_text_and_wait(&text).unwrap();
		return;
	}
	if let Some(why) = no_display() {
		// CI sets this so a silently skipped test cannot pass as green.
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"clipboard round trip would be skipped: {why}"
		);
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

	let mut writers = Vec::new();
	for (name, text) in [
		("unicode", unicode),
		("crlf", crlf),
		("mixed newlines", mixed),
		("1 MB", large.as_str()),
	] {
		writers.push(spawn_writer(text));
		// The child takes ownership asynchronously; poll until its text arrives.
		let deadline = Instant::now() + Duration::from_secs(10);
		let back = loop {
			let back = clip::read_text().unwrap_or_default();
			if back == text || Instant::now() > deadline {
				break back;
			}
			std::thread::sleep(Duration::from_millis(50));
		};
		assert!(
			back == text,
			"{name}: {} bytes in, {} bytes out",
			text.len(),
			back.len()
		);
	}

	drop(writers);
	if let Some(saved) = saved {
		let _ = clip::write_text(&saved);
	}
}
