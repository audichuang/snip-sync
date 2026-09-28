#![cfg(target_os = "linux")]
//! Automated real-window OS input driver smoke and integration test for snip-desktop-native.
//!
//! Every click targets bounds the app reports for the rendered control
//! (`[APP:CTRL_BOUNDS: id=...]`, only emitted when the test opts in with
//! `SNIP_NATIVE_E2E=1`), never hard-coded coordinates, and every clicked
//! control is asserted to lie fully inside the window. Controls the app
//! reports as gone (`[APP:CTRL_GONE: id=...]`) are forgotten, so a stale
//! position is never clicked.
//!
//! Note on platforms: On non-Linux platforms (macOS / Windows), interactive OS input
//! driving via X11 xdotool is unvalidated on this milestone; compilation is verified
//! via standard cargo check/build. No empty test is advertised as passed on non-Linux.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use snip_core::clip;
use snip_core::commits;

fn clip_set(text: &str) {
	let mut child = Command::new("xclip")
		.args(["-selection", "clipboard"])
		.stdin(Stdio::piped())
		.spawn()
		.expect("xclip");
	child
		.stdin
		.take()
		.unwrap()
		.write_all(text.as_bytes())
		.unwrap();
	assert!(child.wait().unwrap().success(), "xclip set failed");
}

fn clip_get() -> String {
	let out = Command::new("xclip")
		.args(["-selection", "clipboard", "-o"])
		.output()
		.expect("xclip");
	assert!(out.status.success(), "xclip read failed");
	String::from_utf8(out.stdout).unwrap()
}

/// Latest control bounds (x, y, w, h in physical pixels) keyed by control id.
type Bounds = Arc<Mutex<HashMap<String, [i32; 4]>>>;
type Viewport = Arc<Mutex<(i32, i32)>>;

/// How long a confirmed paste write is held back in E2E mode, so the test
/// can prove the confirmation controls are locked while it runs.
const APPLY_DELAY_MS: u64 = 2500;

fn parse_bounds(line: &str) -> Option<(String, [i32; 4])> {
	let rest = line.split("[APP:CTRL_BOUNDS: id=").nth(1)?;
	let (id, nums) = rest.rsplit_once(" x=")?;
	let mut vals = nums.trim_end_matches(']').split(' ');
	let x = vals.next()?.parse().ok()?;
	let mut v = [x, 0, 0, 0];
	for slot in &mut v[1..] {
		*slot = vals.next()?.split('=').nth(1)?.parse().ok()?;
	}
	Some((id.to_string(), v))
}

fn parse_gone(line: &str) -> Option<String> {
	let rest = line.split("[APP:CTRL_GONE: id=").nth(1)?;
	Some(rest.strip_suffix(']')?.to_string())
}

fn parse_viewport(line: &str) -> Option<(i32, i32)> {
	let rest = line.split("[APP:VIEWPORT: ").nth(1)?;
	let (w, h) = rest.trim_end_matches(']').split_once('x')?;
	Some((w.parse().ok()?, h.parse().ok()?))
}

#[test]
fn bounds_line_parsing() {
	let (id, v) = parse_bounds(
		"[APP:CTRL_BOUNDS: id=paste-overwrite:長路徑/a b.txt x=12 y=-3 w=100 h=20]",
	)
	.unwrap();
	assert_eq!(id, "paste-overwrite:長路徑/a b.txt");
	assert_eq!(v, [12, -3, 100, 20]);
	assert_eq!(parse_viewport("[APP:VIEWPORT: 900x600]"), Some((900, 600)));
	assert_eq!(
		parse_gone("[APP:CTRL_GONE: id=btn-apply]").as_deref(),
		Some("btn-apply")
	);
}

/// Kills the app on drop so a failing assertion never leaks a window.
struct App {
	child: Option<Child>,
	rx: Receiver<String>,
	pid: u32,
}

impl Drop for App {
	fn drop(&mut self) {
		let _ = Command::new("xdotool")
			.args(["keyup", "ctrl", "alt", "shift", "super", "q"])
			.status();
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

/// Starts the app. `probes` = Some(..) opts into E2E instrumentation and
/// feeds reported bounds/viewport into the given maps.
fn spawn_app(
	ws: &Path,
	dest: &Path,
	probes: Option<(Bounds, Viewport)>,
) -> App {
	spawn_app_themed(ws, dest, probes, "dark")
}

fn spawn_app_themed(
	ws: &Path,
	dest: &Path,
	probes: Option<(Bounds, Viewport)>,
	theme: &str,
) -> App {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip-desktop-native"));
	cmd.args([
		"--workspace",
		&ws.to_string_lossy(),
		"--restore-dir",
		&dest.to_string_lossy(),
	])
	.stdout(Stdio::piped())
	.stderr(Stdio::piped())
	.env("XMODIFIERS", "@im=none")
	// Pixel checks below assume the DARK palette.
	.env("SNIP_THEME", theme)
	.env_remove("SNIP_NATIVE_E2E")
	.env_remove("SNIP_NATIVE_E2E_APPLY_DELAY_MS");
	if probes.is_some() {
		cmd.env("SNIP_NATIVE_E2E", "1")
			.env("SNIP_NATIVE_E2E_APPLY_DELAY_MS", APPLY_DELAY_MS.to_string());
	}
	let mut child = cmd.spawn().expect("native desktop should run");
	let pid = child.id();

	let stdout = child.stdout.take().unwrap();
	let (tx, rx) = std::sync::mpsc::channel::<String>();
	std::thread::spawn(move || {
		use std::io::{BufRead, BufReader};
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if let Some((bounds, viewport)) = &probes {
				if let Some((id, v)) = parse_bounds(&line) {
					bounds.lock().unwrap().insert(id, v);
					continue; // high volume; keep the log readable
				}
				if let Some(id) = parse_gone(&line) {
					bounds.lock().unwrap().remove(&id);
					continue;
				}
				if let Some(vp) = parse_viewport(&line) {
					*viewport.lock().unwrap() = vp;
				}
			}
			println!("[APP STDOUT] {line}");
			let _ = tx.send(line);
		}
	});
	let stderr = child.stderr.take().unwrap();
	std::thread::spawn(move || {
		use std::io::{BufRead, BufReader};
		for line in BufReader::new(stderr).lines().map_while(Result::ok) {
			eprintln!("[APP STDERR] {line}");
		}
	});
	App {
		child: Some(child),
		rx,
		pid,
	}
}

/// Collects every stdout line up to and including the first that contains
/// `pattern`, so callers can also assert what did NOT happen meanwhile.
fn lines_until(
	rx: &Receiver<String>,
	pattern: &str,
	timeout: Duration,
) -> Result<Vec<String>, String> {
	let deadline = Instant::now() + timeout;
	let mut seen = Vec::new();
	while Instant::now() < deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
			Ok(line) => {
				let hit = line.contains(pattern);
				seen.push(line);
				if hit {
					return Ok(seen);
				}
			}
			Err(RecvTimeoutError::Timeout) => continue,
			Err(RecvTimeoutError::Disconnected) => {
				return Err(format!(
					"stdout closed while waiting for '{pattern}'"
				));
			}
		}
	}
	Err(format!(
		"timed out after {timeout:?} waiting for '{pattern}'; saw {seen:?}"
	))
}

fn find_wid(pid: u32) -> String {
	let start = Instant::now();
	while start.elapsed() < Duration::from_secs(5) {
		let out = Command::new("xdotool")
			.args(["search", "--pid", &pid.to_string()])
			.output()
			.expect("xdotool must run");
		if let Some(w) = String::from_utf8_lossy(&out.stdout)
			.lines()
			.next()
			.map(|l| l.trim().to_string())
			.filter(|l| !l.is_empty())
		{
			return w;
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	panic!("xdotool must find the window ID for pid {pid}");
}

fn key(wid: &str, keys: &str) {
	let _ = Command::new("xdotool")
		.args(["windowmap", "--sync", wid])
		.status();
	let st = Command::new("xdotool")
		.args(["windowfocus", "--sync", wid])
		.status()
		.expect("xdotool windowfocus must run");
	assert!(st.success(), "xdotool windowfocus failed for window {wid}");
	std::thread::sleep(Duration::from_millis(50));
	let st = Command::new("xdotool")
		.args(["key", "--window", wid, keys])
		.status()
		.expect("xdotool key failed");
	assert!(st.success(), "xdotool key failed for {keys}");
}

/// Ctrl+Q must exit the app cleanly (status 0) within the deadline. A hang
/// or crash fails the test after the process is killed and reaped.
fn quit_cleanly(app: &mut App, wid: &str) {
	let _ = Command::new("xdotool")
		.args(["windowmap", "--sync", wid])
		.status();
	let st = Command::new("xdotool")
		.args(["windowfocus", "--sync", wid])
		.status()
		.expect("xdotool windowfocus must run");
	assert!(st.success(), "xdotool windowfocus failed for window {wid}");
	let _ = Command::new("xdotool")
		.args(["key", "--window", wid, "ctrl+q"])
		.status();
	let _ = Command::new("xdotool")
		.args(["keyup", "ctrl", "alt", "shift", "super", "q"])
		.status();
	let mut child = app.child.take().expect("app still owned");
	let deadline = Instant::now() + Duration::from_secs(5);
	loop {
		match child.try_wait().expect("try_wait") {
			Some(status) => {
				assert!(
					status.success(),
					"app exited uncleanly on Ctrl+Q: {status}"
				);
				return;
			}
			None if Instant::now() < deadline => {
				std::thread::sleep(Duration::from_millis(50));
			}
			None => {
				let _ = child.kill();
				let _ = child.wait();
				panic!("app did not exit within 5s of Ctrl+Q (killed)");
			}
		}
	}
}

#[test]
fn native_desktop_smoke_and_clipboard_verification() {
	// Linux X11 driver strictly requires DISPLAY (Wayland-only without X11 DISPLAY cannot drive xdotool).
	let has_display = std::env::var_os("DISPLAY").is_some();
	if !has_display {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		eprintln!(
			"Skipping native desktop smoke test: no X11 DISPLAY available"
		);
		return;
	}

	let xdotool_check = Command::new("xdotool").arg("--version").output();
	if xdotool_check.is_err() || !xdotool_check.unwrap().status.success() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		eprintln!("Skipping native desktop smoke test: xdotool not available");
		return;
	}
	let _gui = gui_lock();

	let temp_ws = tempfile::tempdir().unwrap();
	let ws_path = temp_ws.path();

	let run = |dir: &Path, args: &[&str]| {
		let status = Command::new("git")
			.current_dir(dir)
			.args(args)
			.status()
			.expect("git must run");
		assert!(status.success(), "git command failed: {:?}", args);
	};

	// ── Fixture Setup: Realistic DAG with fork, join, and unmerged tip ──
	// Repo 1: repo-a
	let r1 = ws_path.join("repo-a");
	fs::create_dir(&r1).unwrap();
	run(&r1, &["init"]);
	run(&r1, &["config", "user.name", "Tester"]);
	run(&r1, &["config", "user.email", "test@example.com"]);

	// Base commit: alpha.txt, common.txt, unchanged.txt, and deleted_later.txt
	fs::write(r1.join("alpha.txt"), "hello from alpha base\n").unwrap();
	fs::write(r1.join("common.txt"), "common file base\n").unwrap();
	fs::write(
		r1.join("unchanged.txt"),
		"this is unchanged tracked file content\n",
	)
	.unwrap();
	fs::write(
		r1.join("deleted_later.txt"),
		"content of deleted file from earlier commit\n",
	)
	.unwrap();
	run(
		&r1,
		&[
			"add",
			"alpha.txt",
			"common.txt",
			"unchanged.txt",
			"deleted_later.txt",
		],
	);
	run(&r1, &["commit", "-m", "init repo-a base"]);
	run(&r1, &["branch", "-M", "main"]);

	let base_sha_raw = Command::new("git")
		.current_dir(&r1)
		.args(["rev-parse", "HEAD"])
		.output()
		.unwrap();
	let base_commit_sha = String::from_utf8(base_sha_raw.stdout)
		.unwrap()
		.trim()
		.to_string();

	// Branch 1: feature/merge-fixture
	run(&r1, &["checkout", "-b", "feature/merge-fixture"]);
	fs::write(r1.join("feature.txt"), "feature branch content\n").unwrap();
	run(&r1, &["add", "feature.txt"]);
	run(&r1, &["commit", "-m", "feat: add feature.txt"]);

	// Branch 2: feature/unmerged-tip (extra unmerged tip commit)
	run(&r1, &["checkout", "main"]);
	run(&r1, &["checkout", "-b", "feature/unmerged-tip"]);
	fs::write(r1.join("unmerged.txt"), "unmerged tip content\n").unwrap();
	run(&r1, &["add", "unmerged.txt"]);
	run(&r1, &["commit", "-m", "feat: unmerged tip commit"]);

	// Merge feature/merge-fixture into main (creates merge commit with fork-and-join DAG)
	run(&r1, &["checkout", "main"]);
	run(
		&r1,
		&[
			"merge",
			"--no-ff",
			"-m",
			"Merge branch feature/merge-fixture",
			"feature/merge-fixture",
		],
	);
	let merge_sha_raw = Command::new("git")
		.current_dir(&r1)
		.args(["rev-parse", "HEAD"])
		.output()
		.unwrap();
	let merge_commit_sha = String::from_utf8(merge_sha_raw.stdout)
		.unwrap()
		.trim()
		.to_string();

	// Delete deleted_later.txt in main so it does not exist in working tree
	run(&r1, &["rm", "deleted_later.txt"]);
	run(
		&r1,
		&["commit", "-m", "chore: delete deleted_later.txt in main"],
	);

	// Long Chinese branch name exercises ref label truncation in the log.
	run(
		&r1,
		&[
			"branch",
			"feature/非常長的中文分支名稱-用來檢查版面是否會擠掉按鈕",
		],
	);

	// Create subfolder with nested file for File Explorer tree testing
	let sub = r1.join("subfolder");
	fs::create_dir(&sub).unwrap();
	fs::write(sub.join("nested.txt"), "nested directory file\n").unwrap();

	// Long Chinese path in the working tree for truncation in tree/changes.
	let long_dir =
		r1.join("文件資料夾名稱非常非常長/第二層也是很長的中文目錄名稱");
	fs::create_dir_all(&long_dir).unwrap();
	fs::write(
		long_dir.join("這是一個檔名非常長的中文說明文件-with-long-suffix.md"),
		"長路徑內容\n",
	)
	.unwrap();

	// Create staged and unstaged dirty changes
	fs::write(r1.join("staged_file.txt"), "staged new file\n").unwrap();
	run(&r1, &["add", "staged_file.txt"]);
	// Dirty unstaged modification on alpha.txt
	fs::write(r1.join("alpha.txt"), "hello from alpha modified\n").unwrap();

	// Repo 2: repo-b
	let r2 = ws_path.join("repo-b");
	fs::create_dir(&r2).unwrap();
	run(&r2, &["init"]);
	run(&r2, &["config", "user.name", "Tester"]);
	run(&r2, &["config", "user.email", "test@example.com"]);
	fs::write(r2.join("beta.txt"), "hello from beta base\n").unwrap();
	run(&r2, &["add", "beta.txt"]);
	run(&r2, &["commit", "-m", "init repo-b"]);
	fs::write(r2.join("beta.txt"), "hello from beta modified\n").unwrap();

	// Output directory for screenshots (SNIP_E2E_OUT or default target/native-e2e-artifacts)
	let out_dir = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			let manifest = PathBuf::from(
				std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default(),
			);
			let ws = manifest
				.parent()
				.and_then(|p| p.parent())
				.unwrap_or(&manifest);
			ws.join("target/native-e2e-artifacts")
		});
	fs::create_dir_all(&out_dir).expect("create artifact output dir");

	// Pre-create destination directory and existing file to verify safe paste guards
	let dest_dir = tempfile::tempdir().unwrap();
	let existing_target = dest_dir.path().join("existing.txt");
	fs::write(&existing_target, "preserve original content on disk").unwrap();

	// ── 0. A normal run (no E2E opt-in) emits no instrumentation ──
	{
		let mut app = spawn_app(ws_path, dest_dir.path(), None);
		let mut seen = lines_until(
			&app.rx,
			"[APP:READY_REPOS: 2]",
			Duration::from_secs(6),
		)
		.expect("normal run must become ready");
		let wid = find_wid(app.pid);
		// Navigate between tool windows; this redraws many controls.
		for keys in ["alt+1", "alt+0", "alt+1", "Tab", "Shift+Tab"] {
			key(&wid, keys);
		}
		seen.extend(
			lines_until(
				&app.rx,
				"[APP:TAB_SWITCHED: FileExplorer",
				Duration::from_secs(5),
			)
			.expect("normal run must react to navigation"),
		);
		std::thread::sleep(Duration::from_millis(300));
		seen.extend(app.rx.try_iter());
		let leaked: Vec<_> = seen
			.iter()
			.filter(|l| {
				l.contains("CTRL_BOUNDS")
					|| l.contains("CTRL_GONE")
					|| l.contains("VIEWPORT")
			})
			.collect();
		assert!(
			leaked.is_empty(),
			"normal run emitted E2E probes: {leaked:?}"
		);
		quit_cleanly(&mut app, &wid);
	}

	// Seed clipboard with a unique stale sentinel
	let sentinel = format!(
		"SENTINEL_SMOKE_{}",
		SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap()
			.as_nanos()
	);
	clip::write_text(&sentinel).expect("must be able to seed system clipboard");

	// ── E2E run with instrumentation opted in ──
	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app = spawn_app(
		ws_path,
		dest_dir.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	let rx = &app.rx;
	let wait_for_pattern =
		|pattern: &str, timeout: Duration| -> Result<String, String> {
			lines_until(rx, pattern, timeout).map(|mut v| v.pop().unwrap())
		};

	// 1. Wait for app readiness (deadline: 6s)
	wait_for_pattern("[APP:READY_REPOS: 2]", Duration::from_secs(6))
		.expect("app should report 2 repos ready within deadline");

	// 2. Discover X11 window ID for the app PID
	let wid = find_wid(app.pid);

	let activate = || {
		let _ = Command::new("xdotool")
			.args(["windowmap", "--sync", &wid])
			.status();
		let st = Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status()
			.expect("xdotool windowfocus must run");
		assert!(st.success(), "xdotool windowfocus failed for window {wid}");
		std::thread::sleep(Duration::from_millis(50));
	};
	let send_key = |keys: &str| key(&wid, keys);

	let xdo = |args: &[&str]| {
		let st = Command::new("xdotool")
			.args(args)
			.status()
			.expect("xdotool must run");
		assert!(st.success(), "xdotool {args:?} failed");
	};

	// Returns the latest reported bounds for `id`, asserting the control is
	// currently drawn and fully inside the viewport (reachable, not clipped).
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0 && v[3] > 0,
					"control {id} has empty bounds {v:?}"
				);
				assert!(
					v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"control {id} bounds {v:?} exceed viewport {vw}x{vh}"
				);
				return v;
			}
			assert!(
				Instant::now() < deadline,
				"control {id} is not drawn (never reported or gone)"
			);
			std::thread::sleep(Duration::from_millis(50));
		}
	};

	// Asserts a control is no longer drawn (the app reported it gone).
	// Polls: the frame that drops the control can land well after the event.
	let absent = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(4);
		while bounds.lock().unwrap().contains_key(id) {
			assert!(
				Instant::now() < deadline,
				"control {id} must not be drawn"
			);
			std::thread::sleep(Duration::from_millis(40));
		}
	};

	let click = |id: &str| {
		// Let the latest frame report its bounds before reading them.
		std::thread::sleep(Duration::from_millis(250));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		println!("[TEST DRIVER] click {id} at ({x}, {y}) bounds {v:?}");
		activate();
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"1",
		]);
	};

	let capture_artifact = |filename: &str| {
		let out_png = out_dir.join(filename);
		let tmp_xwd = tempfile::Builder::new()
			.suffix(".xwd")
			.tempfile()
			.expect("must create tempfile for xwd");
		let tmp_xwd_path = tmp_xwd.path().to_path_buf();

		// Parse absolute client coordinates using xwininfo -id <wid>
		// to correctly handle window manager decorations and reparenting
		// across both bare Xvfb and window managers (e.g. DISPLAY:1).
		let mut crop_rect = None;
		if let Ok(out) = Command::new("xwininfo").args(["-id", &wid]).output() {
			if out.status.success() {
				let s = String::from_utf8_lossy(&out.stdout);
				let mut gx = None;
				let mut gy = None;
				let mut gw = None;
				let mut gh = None;
				let mut is_viewable = false;
				for line in s.lines() {
					let line = line.trim();
					if let Some(val) =
						line.strip_prefix("Absolute upper-left X:")
					{
						gx = val.trim().parse::<i32>().ok();
					} else if let Some(val) =
						line.strip_prefix("Absolute upper-left Y:")
					{
						gy = val.trim().parse::<i32>().ok();
					} else if let Some(val) = line.strip_prefix("Width:") {
						gw = val.trim().parse::<i32>().ok();
					} else if let Some(val) = line.strip_prefix("Height:") {
						gh = val.trim().parse::<i32>().ok();
					} else if line.contains("Map State: IsViewable") {
						is_viewable = true;
					}
				}
				if let (Some(x), Some(y), Some(w), Some(h)) = (gx, gy, gw, gh) {
					if w > 0 && h > 0 {
						crop_rect = Some((x, y, w, h));
					}
				}
				assert!(
					is_viewable,
					"window {wid} must be viewable for artifact capture"
				);
			}
		}

		let mut captured = false;
		if let Some((x, y, w, h)) = crop_rect {
			let root_st = Command::new("xwd")
				.args([
					"-root",
					"-silent",
					"-out",
					tmp_xwd_path.to_str().unwrap(),
				])
				.status();
			if root_st.is_ok_and(|s| s.success()) {
				let crop_arg = format!("{w}x{h}+{x}+{y}");
				let conv_st = Command::new("convert")
					.args([
						tmp_xwd_path.to_str().unwrap(),
						"-crop",
						&crop_arg,
						"+repage",
						out_png.to_str().unwrap(),
					])
					.status();
				if conv_st.is_ok_and(|s| s.success()) && out_png.exists() {
					let len =
						fs::metadata(&out_png).map(|m| m.len()).unwrap_or(0);
					if len > 1024 {
						captured = true;
					}
				}
			}
		}

		if !captured {
			let xwd_st = Command::new("xwd")
				.args([
					"-id",
					&wid,
					"-silent",
					"-out",
					tmp_xwd_path.to_str().unwrap(),
				])
				.status()
				.expect("xwd must be in PATH");
			assert!(xwd_st.success(), "xwd failed for window {wid}");

			let conv_st = Command::new("convert")
				.args([
					tmp_xwd_path.to_str().unwrap(),
					out_png.to_str().unwrap(),
				])
				.status()
				.expect("convert must be in PATH");
			assert!(conv_st.success(), "convert failed for {filename}");
		}

		assert!(
			out_png.exists(),
			"PNG artifact must exist: {}",
			out_png.display()
		);
		let len = fs::metadata(&out_png).expect("stat PNG").len();
		assert!(len > 1024, "PNG artifact must be non-empty (>1KB)");

		println!(
			"[TEST DRIVER] Captured screenshot artifact: {}",
			out_png.display()
		);
	};

	let resize = |w: i32, h: i32| {
		xdo(&["windowsize", "--sync", &wid, &w.to_string(), &h.to_string()]);
		let deadline = Instant::now() + Duration::from_secs(5);
		while *viewport.lock().unwrap() != (w, h) {
			assert!(
				Instant::now() < deadline,
				"viewport did not become {w}x{h}: {:?}",
				*viewport.lock().unwrap()
			);
			std::thread::sleep(Duration::from_millis(50));
		}
		std::thread::sleep(Duration::from_millis(300));
	};

	// Normalize to the reference size in case the WM placed it differently.
	resize(1080, 720);

	// 3. Repo and Ref selector filter + choose with real OS input (no stale preview)
	let filter_repo = |query: &str, excluded_row: &str| {
		// Require a drawn-to-gone transition, not an old filter event or cached row.
		control(excluded_row);
		xdo(&["type", "--window", &wid, query]);
		wait_for_pattern(
			"[APP:SELECTOR_FILTER: items=1]",
			Duration::from_secs(3),
		)
		.expect("repo filter must finish with one candidate");
		// CTRL_GONE ends the frame, after moved row bounds; the stdout reader
		// commits both to this map before discarding those high-volume events.
		let deadline = Instant::now() + Duration::from_secs(3);
		while bounds.lock().unwrap().contains_key(excluded_row) {
			assert!(
				Instant::now() < deadline,
				"filtered-out row {excluded_row} is still drawn"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
	};
	println!("[TEST DRIVER] Opening repo selector and choosing repo-b...");
	click("btn-repo-selector");
	wait_for_pattern("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3))
		.expect("repo selector popover must open");
	click("selector-input");
	filter_repo("repo-b", "pick-repo:repo-a");
	click("pick-repo:repo-b");
	wait_for_pattern(
		"[APP:SELECTOR_CHOSE: repo=repo-b]",
		Duration::from_secs(3),
	)
	.expect("repo-b should be chosen");
	wait_for_pattern("[APP:REPO_LOADED: repo-b", Duration::from_secs(5))
		.expect("repo-b should load");
	wait_for_pattern("[APP:PREVIEW_LOADED: beta.txt]", Duration::from_secs(5))
		.expect("preview must update to repo-b without stale repo-a preview");

	// Switch back to repo-a via selector
	println!("[TEST DRIVER] Switching back to repo-a via selector...");
	click("btn-repo-selector");
	wait_for_pattern("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3))
		.expect("repo selector popover must open");
	click("selector-input");
	filter_repo("repo-a", "pick-repo:repo-b");
	click("pick-repo:repo-a");
	wait_for_pattern(
		"[APP:SELECTOR_CHOSE: repo=repo-a]",
		Duration::from_secs(3),
	)
	.expect("repo-a should be chosen");
	wait_for_pattern("[APP:REPO_LOADED: repo-a", Duration::from_secs(5))
		.expect("repo-a should load");
	wait_for_pattern("[APP:GRAPH_LOADED:", Duration::from_secs(5))
		.expect("graph layout should load");

	// The log merges both repositories; its Repository chip narrows it to
	// repo-a, the single-repository log the checks below were written for.
	click("log-filter-repo");
	click("log-repo:repo-a");
	wait_for_pattern("[APP:LOG_REPOS: n=1]", Duration::from_secs(3))
		.expect("the Repository chip must narrow the log");
	wait_for_pattern("[APP:GRAPH_LOADED: commits=5]", Duration::from_secs(5))
		.expect("repo-a's log must load");

	// Ref selector: test HEAD and all filter
	println!("[TEST DRIVER] Filtering refs via ref selector...");
	click("btn-ref-selector");
	wait_for_pattern("[APP:SELECTOR_OPEN: Ref", Duration::from_secs(3))
		.expect("ref selector popover must open");
	click("pick-ref:HEAD");
	wait_for_pattern("[APP:REF_FILTER: HEAD]", Duration::from_secs(3))
		.expect("HEAD filter must be applied");
	wait_for_pattern("[APP:GRAPH_LOADED: commits=4]", Duration::from_secs(5))
		.expect("HEAD filter must show main commits");

	click("btn-ref-selector");
	wait_for_pattern("[APP:SELECTOR_OPEN: Ref", Duration::from_secs(3))
		.expect("ref selector popover must open");
	click("pick-ref:all");
	wait_for_pattern("[APP:REF_FILTER: all]", Duration::from_secs(3))
		.expect("all refs filter must be applied");
	wait_for_pattern("[APP:GRAPH_LOADED: commits=5]", Duration::from_secs(5))
		.expect("All refs must include all 5 commits");

	// ── Artifact 1: Commit Graph with Merge and Fork Topology ──
	println!("[TEST DRIVER] Capturing graph.png artifact...");
	capture_artifact("graph.png");

	// 4. Keyboard Navigation: panel switching via Alt+1 / Alt+0 and Tab focus traversal
	println!("[TEST DRIVER] Switching to Project tool window (Alt+1)...");
	send_key("alt+1");
	wait_for_pattern("[APP:TAB_SWITCHED: FileExplorer", Duration::from_secs(3))
		.expect("Alt+1 must switch to FileExplorer");

	// Observe the painted focus ring inside each control's own settled bounds,
	// rather than accepting FOCUS next/prev handler logs as proof of focus.
	let capture_focus = |filename: &str, expected: &[(&str, bool)]| {
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			let boxes = loop {
				let boxes: Vec<_> =
					expected.iter().map(|(id, _)| control(id)).collect();
				std::thread::sleep(Duration::from_millis(150));
				if expected
					.iter()
					.zip(&boxes)
					.all(|((id, _), v)| control(id) == *v)
				{
					break boxes;
				}
				assert!(
					Instant::now() < deadline,
					"focus target bounds did not settle"
				);
			};
			capture_artifact(filename);
			let mut presented = true;
			for ((id, focused), v) in expected.iter().zip(boxes) {
				assert_eq!(control(id), v, "focus target moved during capture");
				// The absolute child probe covers the padding box, inside the
				// declared border_1. Include that border at this 1x X11 scale.
				let [x, y, w, h] = [v[0] - 1, v[1] - 1, v[2] + 2, v[3] + 2];
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(x >= 0 && y >= 0 && x + w <= vw && y + h <= vh);
				let crop = format!("{w}x{h}+{x}+{y}");
				let out = Command::new("convert")
				.arg(out_dir.join(filename))
				.args([
					"-crop",
					&crop,
					"+repage",
					"-alpha",
					"off",
					"-fx",
					// The visible keyboard ring is DARK.focus_ring #3871e1; allow one level
					// of raster rounding in the captured sRGB channels.
					"abs(r-56/255)<0.006 && abs(g-113/255)<0.006 && abs(b-225/255)<0.006 ? 1 : 0",
					"-format",
					"%[fx:mean]",
					"info:",
				])
				.output()
				.expect("convert focus crop must run");
				assert!(
					out.status.success(),
					"focus crop failed: {:?}",
					out.stderr
				);
				let fraction: f64 = String::from_utf8(out.stdout)
					.unwrap()
					.trim()
					.parse()
					.unwrap();
				let pixels = fraction * f64::from(w * h);
				println!("[TEST DRIVER] focus {id} bounds={v:?} crop={crop} pixels={pixels} expected={focused}");
				let matches = if *focused {
					pixels >= 8.0
				} else {
					pixels < 1.0
				};
				assert!(
				matches || Instant::now() < deadline,
				"{id} focused={focused}, but its own crop contains {pixels} focus-ring pixels; {filename}"
			);
				presented &= matches;
			}
			if presented {
				break;
			}
			// Focus does not move control bounds. Await its painted state without
			// sending another key; persistent wrong focus still fails this deadline.
		}
	};

	click("btn-locale");
	wait_for_pattern("[APP:LOCALE: En]", Duration::from_secs(3))
		.expect("harmless locale focus anchor must switch to English");
	// IntelliJ draws no focus ring after a mouse click, only after keys.
	capture_focus("keyboard-anchor-locale.png", &[("btn-locale", false)]);
	println!("[TEST DRIVER] Disabled Copy must neither focus nor activate...");
	// The basket is still empty. Clicking the disabled button must not focus it;
	// Enter/Space may retain the harmless locale focus, but cannot invoke Copy.
	let _: Vec<_> = rx.try_iter().collect();
	click("btn-copy");
	send_key("Return");
	send_key("space");
	capture_focus("keyboard-disabled-copy.png", &[("btn-copy", false)]);
	assert_eq!(clip_get(), sentinel, "disabled Copy changed the clipboard");
	let disabled_events: Vec<_> = rx.try_iter().collect();
	assert!(
		!disabled_events
			.iter()
			.any(|line| line.contains("[APP:COPY")),
		"disabled Copy dispatched an action: {disabled_events:?}"
	);

	// A real click gives us a known focus anchor. From locale, traverse back
	// through Refresh and Paste to Ref: disabled Copy (tab index 3) is skipped.
	click("btn-locale");
	wait_for_pattern("[APP:LOCALE: ZhTw]", Duration::from_secs(3))
		.expect("locale anchor click must restore Chinese");
	send_key("Shift+Tab");
	capture_focus(
		"keyboard-refresh-focus.png",
		&[("btn-refresh", true), ("btn-locale", false)],
	);
	send_key("Shift+Tab");
	capture_focus(
		"keyboard-paste-focus.png",
		&[("btn-paste", true), ("btn-copy", false)],
	);
	send_key("Shift+Tab");
	capture_focus(
		"keyboard-ref-focus.png",
		&[("btn-ref-selector", true), ("btn-copy", false)],
	);
	send_key("Tab");
	capture_focus(
		"keyboard-forward-skip-copy.png",
		&[
			("btn-paste", true),
			("btn-ref-selector", false),
			("btn-copy", false),
		],
	);
	send_key("Tab");
	send_key("Tab");
	capture_focus("keyboard-locale-focus.png", &[("btn-locale", true)]);
	let _: Vec<_> = rx.try_iter().collect();
	send_key("Return");
	wait_for_pattern("[APP:LOCALE: En]", Duration::from_secs(3))
		.expect("Enter on the focused locale button must change the language");
	let _: Vec<_> = rx.try_iter().collect();
	send_key("space");
	wait_for_pattern("[APP:LOCALE: ZhTw]", Duration::from_secs(3))
		.expect("Space on the focused locale button must change the language");

	// Tab reaches another real control. Keyboard activation must collapse and
	// reopen the Project panel, leaving the existing tree smoke ready to run.
	send_key("Tab");
	capture_focus(
		"keyboard-project-focus.png",
		&[("rail-project", true), ("btn-locale", false)],
	);
	let _: Vec<_> = rx.try_iter().collect();
	send_key("space");
	wait_for_pattern(
		"[APP:TAB_SWITCHED: FileExplorer visible=false",
		Duration::from_secs(3),
	)
	.expect("Space on focused Project must collapse the panel");
	let _: Vec<_> = rx.try_iter().collect();
	send_key("Return");
	wait_for_pattern(
		"[APP:TAB_SWITCHED: FileExplorer visible=true",
		Duration::from_secs(3),
	)
	.expect("Enter on focused Project must reopen the panel");
	assert_eq!(
		clip_get(),
		sentinel,
		"keyboard navigation changed the clipboard"
	);

	// 5. Expand 'subfolder' by clicking its real tree row
	println!("[TEST DRIVER] Expanding 'subfolder' directory...");
	click("tree-row:subfolder");
	wait_for_pattern("[APP:TREE_EXPANDED: subfolder]", Duration::from_secs(3))
		.expect("subfolder must expand");

	// 6. Select 'subfolder/nested.txt'
	println!("[TEST DRIVER] Selecting 'subfolder/nested.txt'...");
	click("tree-row:subfolder/nested.txt");
	wait_for_pattern(
		"[APP:TREE_FILE_SELECTED: subfolder/nested.txt]",
		Duration::from_secs(3),
	)
	.expect("nested.txt must be selected");
	wait_for_pattern(
		"[APP:PREVIEW_LOADED: subfolder/nested.txt]",
		Duration::from_secs(3),
	)
	.expect("nested.txt content preview must load");

	// Expand the long Chinese path so the tree screenshot shows truncation.
	click("tree-row:文件資料夾名稱非常非常長");
	wait_for_pattern(
		"[APP:TREE_EXPANDED: 文件資料夾名稱非常非常長]",
		Duration::from_secs(3),
	)
	.expect("long Chinese folder must expand");
	std::thread::sleep(Duration::from_millis(300));

	// ── Artifact 2: File Explorer Tree with Expanded Directory & Selection ──
	println!("[TEST DRIVER] Capturing file_tree.png artifact...");
	capture_artifact("file_tree.png");

	// 7. Historical commit tree/blob of deleted file without checkout, source breadcrumb & oracle
	println!("[TEST DRIVER] Selecting earlier commit with deleted file...");
	let base_short = &base_commit_sha[..7];
	click(&format!("commit-row:{base_short}"));
	wait_for_pattern(
		&format!("[APP:COMMIT_SELECTED: {base_short}]"),
		Duration::from_secs(3),
	)
	.expect("base commit must be selected in log");

	// Click 'Browse Tree' button to inspect historical tree without checking out
	println!("[TEST DRIVER] Browsing historical commit tree...");
	wait_for_pattern(
		&format!("[APP:E2E_PREVIEW: source=commit_diff rev={base_commit_sha} "),
		Duration::from_secs(3),
	)
	.expect("selected commit preview must finish before targeting its toolbar");
	click(&format!("btn-browse-tree:{base_commit_sha}"));
	wait_for_pattern(
		&format!("[APP:REV_TREE: {base_short}]"),
		Duration::from_secs(3),
	)
	.expect("commit tree must be loaded for historical SHA");

	// deleted_later.txt is present in historical tree, but absent on disk!
	assert!(
		!r1.join("deleted_later.txt").exists(),
		"deleted_later.txt must NOT exist in working tree"
	);
	println!("[TEST DRIVER] Selecting deleted_later.txt from commit tree...");
	click("rev-row:deleted_later.txt");
	wait_for_pattern(
		"[APP:TREE_FILE_SELECTED: deleted_later.txt]",
		Duration::from_secs(3),
	)
	.expect("historical file must be selected");
	wait_for_pattern(
		"[APP:PREVIEW_LOADED: deleted_later.txt]",
		Duration::from_secs(3),
	)
	.expect("historical blob must load without checkout");

	// Source identifying breadcrumb
	control("breadcrumb");
	control("source-badge");

	// Copy preview and verify against byte-exact Git oracle
	println!("[TEST DRIVER] Copying historical preview and verifying against Git oracle...");
	click("btn-copy-view");
	wait_for_pattern("[APP:PREVIEW_COPIED:", Duration::from_secs(3))
		.expect("preview must be copied");
	let copied_hist = clip::read_text().expect("must read clipboard");
	let git_oracle_raw = Command::new("git")
		.current_dir(&r1)
		.args(["show", &format!("{base_commit_sha}:deleted_later.txt")])
		.output()
		.expect("git show oracle must run");
	let git_oracle_text =
		String::from_utf8(git_oracle_raw.stdout).expect("utf-8 oracle");
	assert_eq!(
		copied_hist, git_oracle_text,
		"copied historical file must match Git oracle byte-for-byte"
	);

	// Return from historical tree to working copy
	click("btn-leave-tree");
	wait_for_pattern("[APP:REV_TREE: off]", Duration::from_secs(3))
		.expect("leaving rev tree must return to working tree");

	// 8. File Find, Goto Line, and Reader selection copy with real OS inputs
	println!("[TEST DRIVER] Testing File Find (Ctrl+F) with real OS input...");
	click("tree-row:subfolder/nested.txt");
	wait_for_pattern(
		"[APP:PREVIEW_LOADED: subfolder/nested.txt]",
		Duration::from_secs(3),
	)
	.expect("nested.txt preview must load");
	send_key("ctrl+f");
	xdo(&["type", "--window", &wid, "nested"]);
	wait_for_pattern("[APP:FIND: matches=1", Duration::from_secs(3))
		.expect("find must report 1 match for 'nested'");
	click("btn-find-next");
	wait_for_pattern("[APP:FIND_AT: 1/1 line=1]", Duration::from_secs(3))
		.expect("find next must step forward");

	println!("[TEST DRIVER] Testing Goto Line (Ctrl+G)...");
	send_key("ctrl+g");
	xdo(&["type", "--window", &wid, "1"]);
	send_key("Return");
	wait_for_pattern("[APP:GOTO: line=1]", Duration::from_secs(3))
		.expect("goto must navigate to line 1");

	// Reader text copy
	send_key("ctrl+c");
	wait_for_pattern("[APP:SELECTION_COPIED:", Duration::from_secs(3))
		.expect("selection must be copied from reader");
	let reader_copied = clip::read_text().expect("clipboard must have text");
	assert!(
		reader_copied.contains("nested directory file"),
		"reader copy must contain nested file content"
	);

	// 9. Inline and Side-by-Side Diff toggle
	println!(
		"[TEST DRIVER] Switching to Changes (Alt+0) and toggling diff mode..."
	);
	send_key("alt+0");
	wait_for_pattern("[APP:TAB_SWITCHED: GitChanges", Duration::from_secs(3))
		.expect("Alt+0 must switch to GitChanges");
	// Verify grouped changes section headers and source-specific probes
	control("change-header:staged");
	control("change-header:unstaged");
	control("change-row:staged:staged_file.txt");
	control("change-row:unstaged:alpha.txt");

	println!("[TEST DRIVER] Selecting staged file to verify staged diff source badge...");
	click("change-row:staged:staged_file.txt");
	wait_for_pattern(
		"[APP:E2E_PREVIEW: source=staged_changes",
		Duration::from_secs(3),
	)
	.expect("staged_file.txt must report source=staged_changes");
	wait_for_pattern(
		"[APP:PREVIEW_LOADED: staged_file.txt]",
		Duration::from_secs(3),
	)
	.expect("staged_file.txt diff preview must load");

	println!("[TEST DRIVER] Selecting unstaged file to verify unstaged diff source badge...");
	click("change-row:alpha.txt");
	wait_for_pattern(
		"[APP:E2E_PREVIEW: source=unstaged_changes",
		Duration::from_secs(3),
	)
	.expect("alpha.txt must report source=unstaged_changes");
	wait_for_pattern("[APP:PREVIEW_LOADED: alpha.txt]", Duration::from_secs(3))
		.expect("alpha.txt diff preview must load");

	// Toggle to Side-by-Side diff
	click("btn-diff-mode");
	wait_for_pattern("[APP:DIFF_MODE: SideBySide]", Duration::from_secs(3))
		.expect("diff mode must toggle to SideBySide");

	// Toggle back to Inline diff
	click("btn-diff-mode");
	wait_for_pattern("[APP:DIFF_MODE: Inline]", Duration::from_secs(3))
		.expect("diff mode must toggle back to Inline");

	// 10. Git Log: search by author, HEAD jump, merge collapse, range compare
	println!("[TEST DRIVER] Testing Git Log author search...");
	click("log-filter-user");
	wait_for_pattern("[APP:LOG_MENU: Some(User)]", Duration::from_secs(3))
		.expect("User chip must open its menu");
	click("log-user:Tester");
	wait_for_pattern(
		"[APP:LOG_SEARCH: active=true author=true]",
		Duration::from_secs(3),
	)
	.expect("log search for author must execute");

	// Text search combines with the author filter.
	click("log-search-input");
	xdo(&["type", "--window", &wid, "commit"]);
	send_key("Return");
	wait_for_pattern(
		"[APP:LOG_SEARCH: active=true author=true]",
		Duration::from_secs(3),
	)
	.expect("text search must keep the author filter");
	send_key("Escape");
	wait_for_pattern(
		"[APP:LOG_SEARCH: active=true author=true]",
		Duration::from_secs(3),
	)
	.expect("clearing the text keeps the author filter");

	// Reset author search from the chip's clear button.
	click("log-filter-user-clear");
	wait_for_pattern("[APP:LOG_SEARCH: active=false", Duration::from_secs(3))
		.expect("clearing log search must restore full graph");

	// HEAD Jump
	println!("[TEST DRIVER] Testing HEAD jump button...");
	click("btn-head");
	wait_for_pattern("[APP:HEAD_LOCATED:", Duration::from_secs(3))
		.expect("HEAD button must locate head row in log");

	// Merge collapse
	println!("[TEST DRIVER] Testing merge commit collapse and expansion...");
	let merge_short = &merge_commit_sha[..7];
	click(&format!("collapse:{merge_short}"));
	wait_for_pattern(
		&format!("[APP:MERGE_COLLAPSE: sha={merge_short} collapsed=true"),
		Duration::from_secs(3),
	)
	.expect("merge collapse must hide branched commits");

	// Expand merge back
	click(&format!("collapse:{merge_short}"));
	wait_for_pattern(
		&format!("[APP:MERGE_COLLAPSE: sha={merge_short} collapsed=false"),
		Duration::from_secs(3),
	)
	.expect("expanding merge must restore rows");

	// Range compare: select commit A, Shift-click commit B
	println!("[TEST DRIVER] Testing commit range selection and compare...");
	click(&format!("commit-row:{merge_short}"));
	std::thread::sleep(Duration::from_millis(150));
	let v_base = control(&format!("commit-row:{base_short}"));
	let (bx, by) = (v_base[0] + v_base[2] / 2, v_base[1] + v_base[3] / 2);
	activate();
	xdo(&[
		"mousemove",
		"--window",
		&wid,
		&bx.to_string(),
		&by.to_string(),
		"keydown",
		"Shift_L",
		"click",
		"1",
		"keyup",
		"Shift_L",
	]);
	wait_for_pattern("[APP:RANGE: commits=", Duration::from_secs(3))
		.expect("shift-click must select commit range");

	// Compare button
	click("btn-compare");
	wait_for_pattern("[APP:COMPARE:", Duration::from_secs(3))
		.expect("btn-compare must trigger compare view");

	// 10b. The display range from the merge down to the root includes the
	// side-branch commit. That is not a first-parent chain, so the copy must
	// fail before it replaces the clipboard.
	println!("[TEST DRIVER] Rejecting a non-contiguous commit selection...");
	let before_invalid =
		clip::read_text().expect("clipboard before commit copy");
	click("btn-copy-commits");
	wait_for_pattern("[APP:COPY_COMMITS_ERR:", Duration::from_secs(5))
		.expect("side-branch range must be rejected");
	let after_invalid = clip::read_text().expect("clipboard after rejection");
	assert_eq!(
		before_invalid, after_invalid,
		"rejected commit export must not overwrite the clipboard"
	);
	assert!(
		!commits::is_commit_payload(&after_invalid),
		"rejected export must not become a commit payload"
	);

	// Switch back to Project for file copy workflow
	send_key("alt+1");
	wait_for_pattern("[APP:TAB_SWITCHED: FileExplorer", Duration::from_secs(3))
		.expect("Alt+1 must switch to FileExplorer");

	// Tick the checkbox of unchanged tracked file 'unchanged.txt'
	println!("[TEST DRIVER] Clicking checkbox for unchanged.txt...");
	click("tree-chk:unchanged.txt");
	wait_for_pattern(
		"[APP:TREE_TOGGLED: unchanged.txt]",
		Duration::from_secs(3),
	)
	.expect("unchanged.txt checkbox must be toggled");

	// Switching tool windows must not clear the pending copy selection
	click("rail-changes");
	wait_for_pattern("[APP:TAB_SWITCHED: GitChanges", Duration::from_secs(3))
		.expect("rail must switch to Changes");
	click("rail-project");
	wait_for_pattern(
		"[APP:TAB_SWITCHED: FileExplorer visible=true selected=1]",
		Duration::from_secs(3),
	)
	.expect("Project selection must survive a Changes round trip");

	// Splitters are real drag targets
	for (id, dx, dy, which) in [
		("splitter-left", 60, 0, "Left"),
		("splitter-bottom", 0, -40, "Bottom"),
	] {
		std::thread::sleep(Duration::from_millis(250));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		activate();
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
		]);
		xdo(&["mousedown", "1"]);
		for step in 1..=4 {
			xdo(&[
				"mousemove",
				"--window",
				&wid,
				&(x + dx * step / 4).to_string(),
				&(y + dy * step / 4).to_string(),
			]);
			std::thread::sleep(Duration::from_millis(30));
		}
		xdo(&["mouseup", "1"]);
		let line = wait_for_pattern(
			&format!("[APP:SPLIT_RESIZED: {which}"),
			Duration::from_secs(3),
		)
		.expect("splitter drag must resize the tool window");
		println!("[TEST DRIVER] {line}");
	}

	// 8. Copy selected unchanged tracked file to clipboard (Ctrl+C)
	println!("[TEST DRIVER] Copying unchanged.txt selection (Ctrl+C)...");
	send_key("ctrl+c");
	wait_for_pattern("[APP:COPY_DONE: copied=1]", Duration::from_secs(5))
		.expect("copy selection from tree must report copied=1");

	// Verify clipboard contains exact bytes of unchanged.txt and excludes dirty/staged files
	let clip_text = clip::read_text().expect("must read clipboard text");
	assert!(
		clip_text.contains("unchanged.txt"),
		"clipboard must contain unchanged.txt"
	);
	assert!(
		clip_text.contains("this is unchanged tracked file content"),
		"clipboard must contain exact unchanged file content"
	);
	assert!(
		!clip_text.contains("staged_file.txt"),
		"clipboard must exclude unrelated staged file from git changes tab"
	);
	assert!(
		!clip_text.contains("alpha modified"),
		"clipboard must exclude unrelated dirty file from git changes tab"
	);

	// 8b. Real Clipboard Roundtrip (Own Export -> Destination) & Apply Cancel Verification
	println!("[TEST DRIVER] Triggering paste preview for real own export (Ctrl+V)...");
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW: items=1", Duration::from_secs(5))
		.expect("paste preview should open with own export item=1");

	let roundtrip_dest_file = dest_dir.path().join("unchanged.txt");
	assert!(
		!roundtrip_dest_file.exists(),
		"unchanged.txt must NOT exist on disk during preview"
	);

	// Test Cancel: Press Escape to cancel preview
	println!("[TEST DRIVER] Testing Apply Cancel via Escape...");
	send_key("Escape");
	wait_for_pattern("[APP:PASTE_CANCELLED]", Duration::from_secs(3))
		.expect("paste preview should be cancelled on Escape");
	assert!(
		!roundtrip_dest_file.exists(),
		"unchanged.txt must STILL NOT exist after cancel"
	);

	// Re-trigger paste preview for real own export and Apply
	println!(
		"[TEST DRIVER] Re-triggering paste preview and Applying own export..."
	);
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW: items=1", Duration::from_secs(5))
		.expect("paste preview should open again");
	send_key("Return");
	wait_for_pattern("[APP:PASTE_DONE: created=1", Duration::from_secs(8))
		.expect("paste should finish successfully");
	assert!(
		roundtrip_dest_file.exists(),
		"unchanged.txt must now exist on disk from real export roundtrip"
	);
	let roundtrip_bytes = fs::read_to_string(&roundtrip_dest_file).unwrap();
	assert_eq!(
		roundtrip_bytes.trim(),
		"this is unchanged tracked file content",
		"restored bytes must match exact own export"
	);

	// 9. Multi-Operation Safe Paste Workflow: Create, Overwrite Guard, Delete, Stale Check
	// Pre-create to_delete.txt in destination to verify delete execution
	let to_delete_file = dest_dir.path().join("to_delete.txt");
	fs::write(&to_delete_file, "content to be deleted by plan").unwrap();
	assert!(to_delete_file.exists());

	// The created file uses a long Chinese path to exercise row truncation.
	let new_rel =
		"長路徑資料夾名稱非常長/第二層中文目錄名稱也非常長/新增的檔案名稱很長-new_file.txt";
	let paste_payload = format!(
		"// FILE: {new_rel}\nnew file restored bytes\n// FILE: existing.txt\ndangerous overwrite attempt\n// FILE: [DELETED] to_delete.txt\n// This file has been deleted in this change\n"
	);
	clip::write_text(&paste_payload)
		.expect("must be able to write paste payload");

	// Trigger Multi-Operation Paste Preview via OS input (Ctrl+V)
	println!(
		"[TEST DRIVER] Triggering Multi-Operation Paste Preview (Ctrl+V)..."
	);
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW:", Duration::from_secs(5))
		.expect("paste preview must open");
	click("paste-map-keep:長路徑資料夾名稱非常長");
	let mapped = wait_for_pattern("[APP:PASTE_MAPPED:", Duration::from_secs(3))
		.expect("nested prefix must be confirmed");
	assert!(
		mapped.contains("items=3"),
		"confirmed preview must list 3 items: {mapped}"
	);

	// CRITICAL INVARIANT: Verify destination files are UNTOUCHED before Apply!
	let new_dest_file = dest_dir.path().join(new_rel);
	assert!(
		!new_dest_file.exists(),
		"new file must NOT exist before Apply control is confirmed"
	);
	let existing_before_apply =
		fs::read_to_string(&existing_target).expect("read existing.txt");
	assert_eq!(
		existing_before_apply, "preserve original content on disk",
		"existing.txt must remain completely unchanged before Apply control is confirmed"
	);
	assert!(to_delete_file.exists());

	// Every paste control must be fully inside the 1080x720 window.
	std::thread::sleep(Duration::from_millis(300));
	for id in [
		"btn-apply",
		"btn-cancel",
		"paste-overwrite:existing.txt",
		"paste-include:existing.txt",
		"paste-include:to_delete.txt",
		&format!("paste-include:{new_rel}"),
	] {
		control(id);
	}

	// ── Artifact 3: Paste Preview Panel with Operation Rows, Content & Overwrite Off ──
	println!("[TEST DRIVER] Capturing paste_preview.png artifact...");
	capture_artifact("paste_preview.png");

	// 10. Real mouse click on the overwrite control of existing.txt
	println!("[TEST DRIVER] Clicking overwrite control of existing.txt...");
	click("paste-overwrite:existing.txt");
	wait_for_pattern(
		"[APP:PASTE_TOGGLED: idx=1 state=true]",
		Duration::from_secs(3),
	)
	.expect("row 1 overwrite must be toggled on");

	// 11. Test Staleness Detection: Modify destination file externally behind app's back
	println!("[TEST DRIVER] Simulating external destination modification to verify stale guard...");
	std::thread::sleep(Duration::from_millis(20));
	fs::write(
		&existing_target,
		"external modification between preview and apply",
	)
	.unwrap();

	// Click Apply while destination is stale
	println!(
		"[TEST DRIVER] Clicking Apply button while destination is stale..."
	);
	click("btn-apply");
	wait_for_pattern("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(8))
		.expect("stale destination detection must block apply");

	// Invariant: stale abort must leave target writes and deletes unexecuted!
	assert!(
		!new_dest_file.exists(),
		"new file must NOT be created when stale abort occurs"
	);
	assert!(
		to_delete_file.exists(),
		"to_delete.txt must NOT be deleted when stale abort occurs"
	);
	assert_eq!(
		fs::read_to_string(&existing_target).unwrap(),
		"external modification between preview and apply",
		"stale abort must not overwrite existing.txt"
	);

	// 11b. An invalid clipboard must invalidate the still-open older plan:
	// neither Enter nor the old Apply button may execute it afterwards.
	println!("[TEST DRIVER] Invalid clipboard must disarm the open plan...");
	control("btn-apply"); // the stale plan is still open here
	clip::write_text("not a snip payload at all").unwrap();
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PLAN_CLEARED]", Duration::from_secs(3))
		.expect("new paste must clear the previous plan first");
	wait_for_pattern("[APP:PASTE_ERR:", Duration::from_secs(3))
		.expect("invalid clipboard must be reported");
	absent("btn-apply");
	// Pressing Return when paste preview is not active does not apply any plan
	// because ApplyPaste is strictly scoped to Some("PastePanel").
	send_key("Return");
	std::thread::sleep(Duration::from_millis(500));
	assert!(!new_dest_file.exists(), "invalidated plan must not create");
	assert!(to_delete_file.exists(), "invalidated plan must not delete");
	assert_eq!(
		fs::read_to_string(&existing_target).unwrap(),
		"external modification between preview and apply",
		"invalidated plan must not overwrite"
	);

	// 12. Re-trigger preview with fresh snapshot and apply confirmation
	clip::write_text(&paste_payload).unwrap();
	println!("[TEST DRIVER] Re-triggering fresh paste preview (Ctrl+V)...");
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW:", Duration::from_secs(5))
		.expect("fresh paste preview must open");
	click("paste-map-keep:長路徑資料夾名稱非常長");
	let mapped = wait_for_pattern("[APP:PASTE_MAPPED:", Duration::from_secs(3))
		.expect("fresh nested prefix must be confirmed");
	assert!(mapped.contains("items=3"), "{mapped}");

	// Click Apply with default safe overwrite (overwrite OFF for existing.txt).
	// The write is held back APPLY_DELAY_MS so we can prove, meanwhile, that
	// cancel / row edits / a new paste are refused and a repo switch does not
	// lose the result.
	println!("[TEST DRIVER] Applying confirmed plan (locked while writing)...");
	click("btn-apply");
	wait_for_pattern("[APP:PASTE_APPLYING]", Duration::from_secs(3))
		.expect("apply must start");
	let t0 = Instant::now();
	send_key("Escape");
	wait_for_pattern(
		"[APP:PASTE_BUSY: refused=cancel]",
		Duration::from_secs(2),
	)
	.expect("Escape must be refused while applying");
	click("paste-include:to_delete.txt");
	wait_for_pattern(
		"[APP:PASTE_BUSY: refused=include]",
		Duration::from_secs(2),
	)
	.expect("row include must be locked while applying");
	send_key("ctrl+v");
	wait_for_pattern(
		"[APP:PASTE_BUSY: refused=preview]",
		Duration::from_secs(2),
	)
	.expect("a new paste must not replace the plan while applying");
	click("btn-cancel"); // disabled: must do nothing
	assert!(
		t0.elapsed() < Duration::from_millis(APPLY_DELAY_MS),
		"busy checks must run while the write is still pending"
	);
	let during = lines_until(rx, "[APP:PASTE_DONE:", Duration::from_secs(8))
		.expect("paste restore must report completion after Apply is clicked");
	let done = during.last().unwrap();
	assert!(
		done.contains("created=1 overwritten=0 skipped=1 deleted=1 errors=0"),
		"apply outcome must match the confirmed plan: {done}"
	);
	for l in &during {
		assert!(
			!l.contains("PASTE_CANCELLED")
				&& !l.contains("PASTE_SEL_TOGGLED")
				&& !l.contains("PASTE_PREVIEW"),
			"plan was changed while applying: {l}"
		);
	}

	// CRITICAL INVARIANTS:
	// a) new file created
	assert!(
		new_dest_file.exists(),
		"new file must now exist on disk after Apply"
	);
	let new_file_bytes =
		fs::read_to_string(&new_dest_file).expect("read new file");
	assert_eq!(
		new_file_bytes, "new file restored bytes",
		"new file must match exact contract payload bytes"
	);

	// b) existing.txt preserved (overwrite was off by default)
	let existing_after_apply =
		fs::read_to_string(&existing_target).expect("read existing.txt");
	assert_eq!(
		existing_after_apply, "external modification between preview and apply",
		"existing.txt must NOT be overwritten when overwrite selection is off by default"
	);

	// c) to_delete.txt deleted
	assert!(
		!to_delete_file.exists(),
		"to_delete.txt must be deleted by confirmed plan"
	);
	click("btn-refresh");
	wait_for_pattern("[APP:REPO_LOADED: repo-a", Duration::from_secs(5))
		.expect("repo-a reloads");

	// 13. Small window: 900x600 must keep every control reachable.
	println!("[TEST DRIVER] Resizing to 900x600...");
	resize(900, 600);
	for id in ["btn-copy", "btn-paste", "rail-project", "rail-log"] {
		control(id);
	}
	capture_artifact("workbench_900x600.png");

	// The long directory is an ambiguous prefix. Keep it under the primary
	// root before the two remaining files (the delete target is gone) appear.
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW:", Duration::from_secs(5))
		.expect("small-window paste preview must open");
	click("paste-map-keep:長路徑資料夾名稱非常長");
	let mapped = wait_for_pattern("[APP:PASTE_MAPPED:", Duration::from_secs(3))
		.expect("nested directory must be kept under the primary root");
	assert!(
		mapped.contains("items=2"),
		"confirmed small-window plan must list the two remaining files: {mapped}"
	);
	std::thread::sleep(Duration::from_millis(300));
	for id in [
		"btn-apply",
		"btn-cancel",
		"paste-overwrite:existing.txt",
		&format!("paste-overwrite:{new_rel}"),
	] {
		control(id);
	}
	capture_artifact("paste_preview_900x600.png");
	click("btn-cancel");
	wait_for_pattern("[APP:PASTE_CANCELLED]", Duration::from_secs(3))
		.expect("Cancel button must close the preview without writing");
	absent("btn-apply");
	assert_eq!(
		fs::read_to_string(&existing_target).unwrap(),
		"external modification between preview and apply",
		"cancel must not write"
	);

	// 13b. Overwrite ON really writes the payload bytes, while a row that is
	// explicitly excluded (even with its overwrite on) stays untouched.
	let overwrite_payload = format!(
		"// FILE: {new_rel}\nEXCLUDED ROW MUST NOT BE WRITTEN\n// FILE: existing.txt\ndangerous overwrite attempt\n"
	);
	clip::write_text(&overwrite_payload).unwrap();
	send_key("ctrl+v");
	wait_for_pattern("[APP:PASTE_PREVIEW:", Duration::from_secs(5))
		.expect("paste preview must reopen");
	click("paste-map-keep:長路徑資料夾名稱非常長");
	let mapped = wait_for_pattern("[APP:PASTE_MAPPED:", Duration::from_secs(3))
		.expect("overwrite preview must confirm the nested directory");
	assert!(mapped.contains("items=2"), "{mapped}");
	click(&format!("paste-overwrite:{new_rel}"));
	wait_for_pattern(
		"[APP:PASTE_TOGGLED: idx=0 state=true]",
		Duration::from_secs(3),
	)
	.expect("excluded row's overwrite toggles on");
	click(&format!("paste-include:{new_rel}"));
	wait_for_pattern(
		"[APP:PASTE_SEL_TOGGLED: idx=0 state=false]",
		Duration::from_secs(3),
	)
	.expect("row must be excluded");
	click("paste-overwrite:existing.txt");
	wait_for_pattern(
		"[APP:PASTE_TOGGLED: idx=1 state=true]",
		Duration::from_secs(3),
	)
	.expect("existing.txt overwrite must toggle on");
	click("btn-apply");
	let done = wait_for_pattern("[APP:PASTE_DONE:", Duration::from_secs(8))
		.expect("apply must finish");
	assert!(
		done.contains("created=0 overwritten=1") && done.contains("errors=0"),
		"apply must overwrite exactly the opted-in file: {done}"
	);
	assert_eq!(
		fs::read_to_string(&existing_target).unwrap(),
		"dangerous overwrite attempt",
		"opted-in overwrite must write the exact payload bytes"
	);
	assert_eq!(
		fs::read_to_string(&new_dest_file).unwrap(),
		"new file restored bytes",
		"explicitly excluded row must stay untouched"
	);

	// 13c. English labels are wider; header must still fit at 900x600.
	send_key("alt+l");
	std::thread::sleep(Duration::from_millis(400));
	for id in ["btn-copy", "btn-paste", "btn-refresh", "btn-locale"] {
		control(id);
	}
	capture_artifact("workbench_900x600_en.png");

	// 14. Clean exit via OS input (Ctrl+Q); hang or crash fails the test.
	println!("[TEST DRIVER] Exiting app (Ctrl+Q)...");
	quit_cleanly(&mut app, &wid);

	for name in [
		"graph.png",
		"file_tree.png",
		"paste_preview.png",
		"workbench_900x600.png",
		"paste_preview_900x600.png",
		"workbench_900x600_en.png",
	] {
		assert!(
			out_dir.join(name).exists(),
			"{name} must exist in {out_dir:?}"
		);
	}
	println!("[TEST DRIVER] SUCCESS: all artifacts verified!");
}

fn gui_lock() -> std::sync::MutexGuard<'static, ()> {
	static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
	LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

fn git_ok(dir: &Path, args: &[&str]) {
	let out = Command::new("git")
		.current_dir(dir)
		.args(args)
		.output()
		.expect("git must run");
	assert!(
		out.status.success(),
		"git {args:?} in {}: {}",
		dir.display(),
		String::from_utf8_lossy(&out.stderr)
	);
}

fn git_rev(dir: &Path) -> String {
	let out = Command::new("git")
		.current_dir(dir)
		.args(["rev-parse", "HEAD"])
		.output()
		.unwrap();
	assert!(out.status.success());
	String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn git_author_commit(dir: &Path, message: &str) {
	let out = Command::new("git")
		.current_dir(dir)
		.args(["commit", "-q", "-m", message])
		.env("GIT_AUTHOR_NAME", "Alice")
		.env("GIT_AUTHOR_EMAIL", "alice@example.com")
		.env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05+00:00")
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"commit {message}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

fn capture_window(wid: &str, out_png: &Path) {
	let tmp = tempfile::Builder::new().suffix(".xwd").tempfile().unwrap();
	let xwd = tmp.path();
	let st = Command::new("xwd")
		.args(["-id", wid, "-silent", "-out", xwd.to_str().unwrap()])
		.status()
		.expect("xwd");
	assert!(st.success(), "xwd failed");
	let st = Command::new("convert")
		.args([xwd.to_str().unwrap(), out_png.to_str().unwrap()])
		.status()
		.expect("convert");
	assert!(st.success(), "convert failed for {}", out_png.display());
	assert!(fs::metadata(out_png).unwrap().len() > 1024);
}

/// Scrolling to the end of the log reads the next page into the window. A
/// failed read must leave the rendered rows intact and stop loading until the
/// user scrolls again, then retry the same page rather than combining new
/// commits with old rails or skipping ahead. Pages 2-10 are sliced from the window page 1 fetched, so page 11 is the
/// first that reads Git again; a repository gone missing fails exactly it.
#[test]
fn native_graph_failed_next_page_is_transactional() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();
	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("history");
	fs::create_dir(&repo).unwrap();
	git_ok(&repo, &["init", "-q", "-b", "main"]);
	git_ok(&repo, &["config", "user.name", "Graph Test"]);
	git_ok(&repo, &["config", "user.email", "graph@example.com"]);
	let mut stream = String::new();
	for n in 0..560 {
		let msg = format!("page commit {n}");
		stream.push_str(&format!(
			"commit refs/heads/main\ncommitter Graph Test <graph@example.com> {} +0000\ndata {}\n{msg}\n\n",
			1_700_000_000 + n * 60,
			msg.len()
		));
	}
	let mut import = Command::new("git")
		.current_dir(&repo)
		.args(["fast-import", "--quiet"])
		.stdin(Stdio::piped())
		.spawn()
		.unwrap();
	import
		.stdin
		.take()
		.unwrap()
		.write_all(stream.as_bytes())
		.unwrap();
	assert!(import.wait().unwrap().success());
	let commits = Command::new("git")
		.current_dir(&repo)
		.args(["log", "--format=%H"])
		.output()
		.unwrap();
	assert!(commits.status.success());
	let commits: Vec<String> = String::from_utf8(commits.stdout)
		.unwrap()
		.lines()
		.map(str::to_owned)
		.collect();
	let first = &commits[0][..7];
	let second = &commits[50][..7];
	// A row near the end of the full ten-page window: on screen when the
	// failed eleventh page is attempted and again after the retry.
	let probe_row = format!("commit-row:{}", &commits[494][..7]);
	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app = spawn_app(
		ws.path(),
		ws.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(8))
		.unwrap();
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	let loaded =
		lines_until(&app.rx, "[APP:E2E_LOG:", Duration::from_secs(8)).unwrap();
	assert!(loaded
		.last()
		.unwrap()
		.contains(&format!("mode=graph n=50 first={first} page=1]")));
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				let (w, h) = *viewport.lock().unwrap();
				assert!(
					v[0] >= 0
						&& v[1] >= 0 && v[2] > 0
						&& v[3] > 0 && v[0] + v[2] <= w
						&& v[1] + v[3] <= h,
					"{id} {v:?} outside {w}x{h}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} must be rendered");
			std::thread::sleep(Duration::from_millis(20));
		}
	};
	// There is no pager: scrolling the list to its end reads the next page.
	let wheel = |clicks: &str, button: &str| {
		let [x, y, w, h] = control("log-list");
		assert!(Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&(x + w / 2).to_string(),
				&(y + h / 2).to_string(),
				"click",
				"--repeat",
				clicks,
				"--delay",
				"15",
				button,
				// Off the list again, so no row is drawn hovered.
				"mousemove",
				"--window",
				&wid,
				"0",
				"0",
			])
			.status()
			.unwrap()
			.success());
	};
	// Scrolls until `pattern` is logged (a page read, or its failure).
	let scroll_until =
		|pattern: &str, clicks: &str, button: &str| -> Vec<String> {
			let deadline = Instant::now() + Duration::from_secs(40);
			let mut seen = Vec::new();
			loop {
				wheel(clicks, button);
				match lines_until(&app.rx, pattern, Duration::from_millis(250))
				{
					Ok(lines) => {
						seen.extend(lines);
						return seen;
					}
					Err(_) => assert!(
						Instant::now() < deadline,
						"scrolling never produced {pattern}"
					),
				}
			}
		};
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	// Vulkan's presented pixels live on the root surface under Xvfb; -id can
	// return a black backing pixmap. Use the same root/crop path as full smoke.
	let capture = |path: &Path| {
		let geometry = Command::new("xdotool")
			.args(["getwindowgeometry", "--shell", &wid])
			.output()
			.unwrap();
		assert!(geometry.status.success());
		let text = String::from_utf8(geometry.stdout).unwrap();
		let geometry: HashMap<_, _> = text
			.lines()
			.filter_map(|line| line.split_once('='))
			.collect();
		let crop = format!(
			"{}x{}+{}+{}",
			geometry["WIDTH"], geometry["HEIGHT"], geometry["X"], geometry["Y"]
		);
		let xwd = tempfile::Builder::new().suffix(".xwd").tempfile().unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			assert!(Command::new("xwd")
				.args(["-root", "-silent", "-out"])
				.arg(xwd.path())
				.status()
				.unwrap()
				.success());
			assert!(Command::new("convert")
				.arg(xwd.path())
				.args(["-crop", &crop, "+repage"])
				.arg(path)
				.status()
				.unwrap()
				.success());
			if fs::metadata(path).unwrap().len() > 1024 {
				break;
			}
			assert!(
				Instant::now() < deadline,
				"window must present a nonblank frame"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
	};
	let crop_row = |name: &str, require_error: bool| {
		let image = out.join(format!("graph-admission-{name}.png"));
		let crop = out.join(format!("graph-admission-{name}-row.png"));
		let deadline = Instant::now() + Duration::from_secs(5);
		// Only accept a row whose pixels match the capture before it, so a
		// frame still moving (scroll, banner) is never the evidence.
		let mut previous: Option<([i32; 4], Vec<u8>)> = None;
		loop {
			let row = control(&probe_row);
			let error = require_error.then(|| control("log-error"));
			capture(&image);
			let [x, y, w, h] = row;
			let pixels = Command::new("convert")
				.arg(&image)
				.args([
					"-crop",
					&format!("{w}x{h}+{x}+{y}"),
					"+repage",
					"-depth",
					"8",
					"RGB:-",
				])
				.output()
				.unwrap();
			assert!(pixels.status.success(), "row crop failed");
			let settled = previous
				.as_ref()
				.is_some_and(|(r, p)| *r == row && *p == pixels.stdout);
			previous = Some((row, pixels.stdout));
			let stable = control(&probe_row) == row
				&& error.is_none_or(|rect| control("log-error") == rect);
			// Probes run during prepaint, before Vulkan presents this frame.
			// Wait for the actual error banner, never for a matching row.
			let presented = error.is_none_or(|[x, y, w, h]| {
				let pixels = Command::new("convert")
					.arg(&image)
					.args([
						"-crop",
						&format!("{w}x{h}+{x}+{y}"),
						"+repage",
						"-depth",
						"8",
						"RGB:-",
					])
					.output()
					.unwrap();
				assert!(pixels.status.success(), "error banner crop failed");
				assert_eq!(pixels.stdout.len(), (w * h * 3) as usize);
				let pixels = pixels.stdout.as_chunks::<3>().0;
				// DARK.error_bg #56272b and antialiased DARK.error #f57e84 text.
				pixels.contains(&[86, 39, 43])
					&& pixels.iter().any(|p| {
						p[0].abs_diff(245) <= 16
							&& p[1].abs_diff(126) <= 16
							&& p[2].abs_diff(132) <= 16
					})
			});
			if stable && presented && settled {
				// Inset 1px top and bottom: the edge pixel rows blend with the
				// neighbouring row at a fractional scroll offset, while the
				// text and every rail crossing the row stay inside.
				let (y, h) = (y + 1, h - 2);
				assert!(Command::new("convert")
					.arg(&image)
					.args(["-crop", &format!("{w}x{h}+{x}+{y}"), "+repage"])
					.arg(&crop)
					.status()
					.unwrap()
					.success());
				println!(
					"[TEST DRIVER] graph {name} row={row:?} error={error:?}"
				);
				return crop;
			}
			assert!(
				Instant::now() < deadline,
				"graph {name} frame did not present: row={row:?} error={error:?}"
			);
			std::thread::sleep(Duration::from_millis(150));
		}
	};
	// Pages 2-10 are sliced from the window page 1 fetched: no Git at all.
	// Only this test's disposable repository is touched, never the standard
	// workload: without `.git` every Git call fails until it is put back.
	let dot_git = repo.join(".git");
	let parked = repo.join("git-parked");
	let park = |away: bool| {
		if away {
			fs::rename(&dot_git, &parked).unwrap();
		} else {
			fs::rename(&parked, &dot_git).unwrap();
		}
	};
	park(true);
	for page in 2..=10 {
		let loaded = scroll_until(&format!("page={page}]"), "4", "5");
		let line = loaded
			.iter()
			.find(|l| l.contains("[APP:E2E_LOG:"))
			.expect("page read must log E2E_LOG");
		// The window grows page by page and keeps its first row.
		assert!(
			line.contains(&format!(
				"mode=graph n={} first={first} page={page}]",
				page * 50
			)),
			"{loaded:?}"
		);
	}
	// Page 11 is the first read that needs Git, and it fails.
	let rejected = scroll_until("[APP:HISTORY_ERROR]", "1", "5");
	assert!(!rejected.iter().any(|line| line.contains("[APP:E2E_LOG:")));
	control("log-error");
	let refused = crop_row("refused", true);
	// A failed read stops the automatic loading: no retry loop.
	assert!(
		lines_until(&app.rx, "[APP:HISTORY_ERROR]", Duration::from_secs(1))
			.is_err(),
		"a failed page must not be retried until the user scrolls again"
	);
	park(false);
	// One notch: it re-arms loading, and no further notch moves the rows
	// the comparison is taken from.
	let retried = scroll_until("[APP:E2E_LOG:", "1", "5");
	assert!(
		retried
			.last()
			.unwrap()
			.contains(&format!("mode=graph n=500 first={second} page=11]")),
		"retry must load the real eleventh page and evict the first: {retried:?}"
	);
	control(&probe_row);
	let deadline = Instant::now() + Duration::from_secs(5);
	while bounds.lock().unwrap().contains_key("log-error") {
		assert!(
			Instant::now() < deadline,
			"successful retry must clear the error"
		);
		std::thread::sleep(Duration::from_millis(20));
	}
	// The row the failure was drawn over is unchanged: the refused page
	// attached nothing to the old rails.
	let after = crop_row("retried", false);
	let comparison = Command::new("compare")
		.args(["-metric", "AE"])
		.arg(&refused)
		.arg(&after)
		.arg("null:")
		.output()
		.unwrap();
	assert!(
		comparison.status.success(),
		"prior rendered row text and graph rails must remain identical: {}",
		String::from_utf8_lossy(&comparison.stderr)
	);
	// Scrolling back to the top reads the evicted first page again and
	// drops the last one; the rows on screen stay where they were.
	let back = scroll_until("[APP:E2E_LOG:", "8", "4");
	assert!(
		back.last()
			.unwrap()
			.contains(&format!("mode=graph n=500 first={first} page=10]")),
		"scrolling up must read page 1 back and evict page 11: {back:?}"
	);
	lines_until(
		&app.rx,
		"[APP:LOG_WINDOW: first_page=1 last_page=10 rows=500]",
		Duration::from_secs(2),
	)
	.unwrap();
	let anchor = back
		.iter()
		.find_map(|l| {
			l.split_once("[APP:LOG_ANCHOR: row=")?
				.1
				.split_once(" was=")
				.map(|(r, w)| {
					(
						r.parse::<usize>().unwrap(),
						w.split_whitespace()
							.next()
							.unwrap()
							.parse::<usize>()
							.unwrap(),
					)
				})
		})
		.expect("prepending must re-anchor the rows on screen");
	assert_eq!(anchor.0, anchor.1 + 50, "the top row keeps its place");
	quit_cleanly(&mut app, &wid);
}

/// Real clipboard and git oracles for basket, mapping, and commit replay.
#[test]
fn native_d3_basket_mapping_and_replay() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	if Command::new("xdotool").arg("--version").output().is_err() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let ws_path = ws.path();
	let src_app = ws_path.join("src-app");
	let src_lib = ws_path.join("a-source").join("lib");
	let trap_lib = ws_path.join("z-trap").join("lib");
	let dest_app = ws_path.join("dest-app");
	let dest_lib = ws_path.join("dest-lib");
	let replay = ws_path.join("replay");
	for dir in [&src_app, &src_lib, &trap_lib, &dest_app, &dest_lib, &replay] {
		fs::create_dir_all(dir).unwrap();
		git_ok(dir, &["init", "-q", "-b", "main"]);
		git_ok(dir, &["config", "user.name", "Tester"]);
		git_ok(dir, &["config", "user.email", "test@example.com"]);
	}

	fs::write(src_app.join("both.txt"), "base\n").unwrap();
	fs::write(src_app.join("keep.txt"), "old keep\n").unwrap();
	fs::write(src_app.join("other.txt"), "do not copy\n").unwrap();
	git_ok(&src_app, &["add", "."]);
	git_ok(&src_app, &["commit", "-qm", "src-app base"]);
	fs::write(src_app.join("both.txt"), "INDEX_A\n").unwrap();
	git_ok(&src_app, &["add", "both.txt"]);
	fs::write(src_app.join("both.txt"), "WORK_B\n").unwrap();
	fs::write(src_app.join("keep.txt"), "KEEP_BYTES\n").unwrap();
	fs::write(src_app.join("other.txt"), "UNSELECTED\n").unwrap();

	fs::write(src_lib.join("moved.txt"), "MOVED_BYTES\n").unwrap();
	fs::write(trap_lib.join("untouched.txt"), "trap\n").unwrap();
	git_ok(&trap_lib, &["add", "."]);
	git_ok(&trap_lib, &["commit", "-qm", "trap"]);
	fs::write(dest_app.join("seed.txt"), "seed\n").unwrap();
	git_ok(&dest_app, &["add", "."]);
	git_ok(&dest_app, &["commit", "-qm", "dest seed"]);
	fs::write(dest_lib.join("seed.txt"), "seed\n").unwrap();
	git_ok(&dest_lib, &["add", "."]);
	git_ok(&dest_lib, &["commit", "-qm", "dest-lib seed"]);

	fs::write(replay.join("replay.txt"), "ROOT_BYTES\n").unwrap();
	git_ok(&replay, &["add", "."]);
	git_author_commit(&replay, "root replay");
	let root_sha = git_rev(&replay);
	git_ok(&replay, &["checkout", "-q", "-b", "side"]);
	fs::write(replay.join("tip.txt"), "TIP_BYTES\n").unwrap();
	git_ok(&replay, &["add", "."]);
	git_author_commit(&replay, "tip replay");
	let side_sha = git_rev(&replay);
	git_ok(&replay, &["checkout", "-q", "main"]);
	fs::write(replay.join("main_only.txt"), "MAIN_ONLY\n").unwrap();
	git_ok(&replay, &["add", "."]);
	git_ok(&replay, &["commit", "-qm", "main only"]);
	let main_sha = git_rev(&replay);

	let out_dir = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out_dir).unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app =
		spawn_app(ws_path, &dest_app, Some((bounds.clone(), viewport.clone())));
	let rx = &app.rx;
	let wait_for = |pattern: &str, timeout: Duration| -> String {
		lines_until(rx, pattern, timeout)
			.unwrap_or_else(|e| panic!("{e}"))
			.pop()
			.unwrap()
	};
	wait_for("[APP:READY_REPOS: 6]", Duration::from_secs(8));
	let wid = find_wid(app.pid);
	let send_key = |keys: &str| key(&wid, keys);
	let xdo = |args: &[&str]| {
		let st = Command::new("xdotool").args(args).status().unwrap();
		assert!(st.success(), "{args:?}");
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(4);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(v[2] > 0 && v[3] > 0, "{id} empty {v:?}");
				assert!(
					v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} not drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		std::thread::sleep(Duration::from_millis(200));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let _ = Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status();
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"1",
		]);
	};
	let resize = |w: i32, h: i32| {
		xdo(&["windowsize", "--sync", &wid, &w.to_string(), &h.to_string()]);
		let deadline = Instant::now() + Duration::from_secs(5);
		while *viewport.lock().unwrap() != (w, h) {
			assert!(Instant::now() < deadline, "viewport {w}x{h}");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	resize(1080, 720);

	let choose_repo = |id: &str, loaded: &str| {
		click("btn-repo-selector");
		wait_for("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3));
		click(id);
		wait_for(loaded, Duration::from_secs(6));
	};
	choose_repo("pick-repo:src-app", "[APP:REPO_LOADED: src-app");

	click("change-row:staged:both.txt");
	let staged_preview = wait_for(
		"[APP:E2E_PREVIEW: source=staged_changes",
		Duration::from_secs(4),
	);
	assert!(staged_preview.contains("path=both.txt"));
	click("btn-copy-view");
	wait_for("[APP:PREVIEW_COPIED:", Duration::from_secs(3));
	let staged_view = clip::read_text().unwrap();
	assert!(
		staged_view.contains("INDEX_A"),
		"staged preview must show index bytes: {staged_view}"
	);
	assert!(
		!staged_view.contains("WORK_B"),
		"staged preview must not show the worktree bytes"
	);

	click("change-chk:staged:both.txt");
	wait_for("[APP:BASKET: n=1", Duration::from_secs(3));
	send_key("ctrl+c");
	wait_for("[APP:COPY_DONE: copied=1]", Duration::from_secs(6));
	let staged_copy = clip::read_text().unwrap();
	assert!(staged_copy.contains("INDEX_A\n"), "{staged_copy}");
	assert!(!staged_copy.contains("WORK_B"), "{staged_copy}");
	assert!(!staged_copy.contains("UNSELECTED"), "{staged_copy}");

	click("change-chk:staged:both.txt");
	wait_for("[APP:BASKET: n=0", Duration::from_secs(3));
	click("change-row:unstaged:both.txt");
	let unstaged_preview = wait_for(
		"[APP:E2E_PREVIEW: source=unstaged_changes",
		Duration::from_secs(4),
	);
	assert!(unstaged_preview.contains("path=both.txt"));
	click("btn-copy-view");
	wait_for("[APP:PREVIEW_COPIED:", Duration::from_secs(3));
	let unstaged_view = clip::read_text().unwrap();
	assert!(
		unstaged_view.contains("WORK_B"),
		"unstaged preview must show worktree bytes: {unstaged_view}"
	);
	click("change-chk:unstaged:both.txt");
	wait_for("[APP:BASKET: n=1", Duration::from_secs(3));
	send_key("ctrl+c");
	wait_for("[APP:COPY_DONE: copied=1]", Duration::from_secs(6));
	let unstaged_copy = clip::read_text().unwrap();
	assert!(unstaged_copy.contains("WORK_B\n"), "{unstaged_copy}");
	assert!(!unstaged_copy.contains("INDEX_A"), "{unstaged_copy}");

	click("change-chk:staged:both.txt");
	wait_for("[APP:BASKET_COLLISION:", Duration::from_secs(3));
	let sentinel = clip::read_text().unwrap();
	send_key("ctrl+c");
	wait_for("[APP:COPY_REFUSED: collision]", Duration::from_secs(4));
	assert_eq!(clip::read_text().unwrap(), sentinel);

	click("change-chk:staged:both.txt");
	click("change-chk:unstaged:both.txt");
	wait_for("[APP:BASKET: n=0", Duration::from_secs(3));
	click("change-chk:unstaged:keep.txt");
	wait_for("[APP:BASKET: n=1", Duration::from_secs(3));
	click("btn-repo-selector");
	wait_for("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3));
	click("pick-repo:a-source/lib");
	let browsed = lines_until(
		rx,
		"[APP:REPO_LOADED: a-source/lib",
		Duration::from_secs(6),
	)
	.expect("source lib must load");
	let basket_after_browse = browsed
		.iter()
		.find(|line| line.contains("[APP:BASKET: n=1"))
		.cloned()
		.unwrap_or_default();
	assert!(
		basket_after_browse.contains("keep.txt"),
		"browsing lib must keep the src-app selection: {browsed:?}"
	);
	assert!(
		!basket_after_browse.contains("moved.txt"),
		"browsing must not add hidden selections: {basket_after_browse}"
	);
	click("change-chk:untracked:moved.txt");
	let basket = wait_for("[APP:BASKET: n=2", Duration::from_secs(3));
	assert!(basket.contains("src-app"));
	assert!(basket.contains("keep.txt"));
	assert!(basket.contains("moved.txt"));
	capture_window(&wid, &out_dir.join("d3_basket_1080x720.png"));
	choose_repo("pick-repo:src-app", "[APP:REPO_LOADED: src-app");

	send_key("ctrl+c");
	wait_for("[APP:COPY_DONE: copied=2]", Duration::from_secs(6));
	let exported = clip::read_text().unwrap();
	assert!(exported.contains("KEEP_BYTES\n"), "{exported}");
	assert!(exported.contains("MOVED_BYTES\n"), "{exported}");
	assert!(exported.contains("lib/moved.txt"), "{exported}");
	assert!(!exported.contains("UNSELECTED"), "{exported}");
	assert!(!exported.contains("WORK_B"), "{exported}");

	send_key("ctrl+v");
	let preview_lines =
		lines_until(rx, "[APP:PASTE_PREVIEW:", Duration::from_secs(5))
			.expect("mapping preview");
	assert!(preview_lines.iter().any(|l| l.contains("mapping=false")));
	let mut dest_lib_idx = None;
	let mut trap_idx = None;
	for line in &preview_lines {
		let Some(rest) = line.split("PASTE_MAP_CANDIDATE: ").nth(1) else {
			continue;
		};
		if !rest.contains("prefix=lib ") {
			continue;
		}
		let idx: usize = rest
			.split("idx=")
			.nth(1)
			.and_then(|s| s.split_whitespace().next())
			.unwrap()
			.parse()
			.unwrap();
		let path = rest.split("path=").nth(1).unwrap_or("");
		if path.contains("dest-lib") {
			dest_lib_idx = Some(idx);
		}
		if path.contains("z-trap") {
			trap_idx = Some(idx);
		}
	}
	let dest_lib_idx =
		dest_lib_idx.expect("dest-lib must be a mapping candidate");
	let trap_idx = trap_idx.expect("same-basename trap must stay a candidate");
	assert_ne!(dest_lib_idx, trap_idx);
	control("paste-map-keep:lib");
	control(&format!("paste-map-pick:lib:{dest_lib_idx}"));
	capture_window(&wid, &out_dir.join("d3_mapping_1080x720.png"));
	resize(900, 600);
	control("paste-map-keep:lib");
	capture_window(&wid, &out_dir.join("d3_mapping_900x600.png"));
	resize(1080, 720);
	click(&format!("paste-map-pick:lib:{dest_lib_idx}"));
	let mapped = wait_for("[APP:PASTE_MAPPED:", Duration::from_secs(3));
	assert!(mapped.contains("dest-lib"), "{mapped}");
	assert!(!mapped.contains("z-trap"), "{mapped}");
	click("btn-apply");
	wait_for("[APP:PASTE_DONE:", Duration::from_secs(6));
	assert_eq!(
		fs::read_to_string(dest_app.join("keep.txt")).unwrap(),
		"KEEP_BYTES"
	);
	assert_eq!(
		fs::read_to_string(dest_lib.join("moved.txt")).unwrap(),
		"MOVED_BYTES"
	);
	assert!(!dest_app.join("moved.txt").exists());
	assert!(!dest_app.join("other.txt").exists());
	assert!(!dest_app.join("both.txt").exists());
	assert!(!trap_lib.join("moved.txt").exists());
	assert_eq!(
		fs::read_to_string(trap_lib.join("untouched.txt")).unwrap(),
		"trap\n"
	);
	assert_eq!(
		fs::read_to_string(src_lib.join("moved.txt")).unwrap(),
		"MOVED_BYTES\n"
	);
	assert_eq!(
		fs::read_to_string(src_app.join("both.txt")).unwrap(),
		"WORK_B\n"
	);
	assert_eq!(
		fs::read_to_string(src_app.join("other.txt")).unwrap(),
		"UNSELECTED\n"
	);

	resize(900, 600);
	capture_window(&wid, &out_dir.join("d3_workbench_900x600.png"));

	choose_repo("pick-repo:replay", "[APP:REPO_LOADED: replay");
	click("btn-ref-selector");
	wait_for("[APP:SELECTOR_OPEN: Ref", Duration::from_secs(3));
	click("pick-ref:refs/heads/side");
	wait_for("[APP:E2E_LOG:", Duration::from_secs(6));
	let side_short = &side_sha[..7];
	let root_short = &root_sha[..7];
	click(&format!("commit-row:{side_short}"));
	wait_for("[APP:COMMIT_SELECTED:", Duration::from_secs(4));
	let base = control(&format!("commit-row:{root_short}"));
	let (bx, by) = (base[0] + base[2] / 2, base[1] + base[3] / 2);
	let _ = Command::new("xdotool")
		.args(["windowfocus", "--sync", &wid])
		.status();
	xdo(&[
		"mousemove",
		"--window",
		&wid,
		&bx.to_string(),
		&by.to_string(),
		"keydown",
		"Shift_L",
		"click",
		"1",
		"keyup",
		"Shift_L",
	]);
	wait_for("[APP:RANGE: commits=2]", Duration::from_secs(3));
	click("btn-copy-commits");
	wait_for("[APP:COPY_COMMITS_DONE: commits=2]", Duration::from_secs(6));
	let commit_clip = clip::read_text().unwrap();
	assert!(commits::is_commit_payload(&commit_clip));
	let payload = commits::parse_commit_payload(&commit_clip).unwrap();
	assert_eq!(payload.commits.len(), 2);
	assert!(payload.commits[0].message.contains("root replay"));
	assert!(payload.commits[1].message.contains("tip replay"));
	assert_eq!(payload.commits[0].author_name, "Alice");
	assert_eq!(payload.commits[0].author_email, "alice@example.com");
	assert!(payload.commits.iter().any(|c| c.files.iter().any(|f| {
		f.path == "replay.txt" && f.content.as_deref() == Some("ROOT_BYTES\n")
	})));
	assert!(payload.commits.iter().any(|c| c.files.iter().any(|f| {
		f.path == "tip.txt" && f.content.as_deref() == Some("TIP_BYTES\n")
	})));
	assert!(!commit_clip.contains("MAIN_ONLY"));
	assert!(!commit_clip.contains(&main_sha));

	click("btn-ref-selector");
	wait_for("[APP:SELECTOR_OPEN: Ref", Duration::from_secs(3));
	click("pick-ref:all");
	wait_for("[APP:E2E_LOG:", Duration::from_secs(6));
	click(&format!("commit-row:{}", &main_sha[..7]));
	wait_for("[APP:COMMIT_SELECTED:", Duration::from_secs(4));
	let side_row = control(&format!("commit-row:{side_short}"));
	let (sx, sy) =
		(side_row[0] + side_row[2] / 2, side_row[1] + side_row[3] / 2);
	xdo(&[
		"mousemove",
		"--window",
		&wid,
		&sx.to_string(),
		&sy.to_string(),
		"keydown",
		"Shift_L",
		"click",
		"1",
		"keyup",
		"Shift_L",
	]);
	wait_for("[APP:RANGE:", Duration::from_secs(3));
	click("btn-copy-commits");
	wait_for("[APP:COPY_COMMITS_ERR:", Duration::from_secs(6));
	assert_eq!(clip::read_text().unwrap(), commit_clip);

	let head_before = git_rev(&dest_app);
	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	fs::write(dest_app.join("tip.txt"), "appeared\n").unwrap();
	click("btn-apply");
	let stale = wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(stale.contains("stale_"), "{stale}");
	assert_eq!(git_rev(&dest_app), head_before);
	assert_eq!(
		fs::read_to_string(dest_app.join("tip.txt")).unwrap(),
		"appeared\n"
	);
	assert!(!dest_app.join("replay.txt").exists());
	fs::remove_file(dest_app.join("tip.txt")).unwrap();

	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	git_ok(&dest_app, &["commit", "-q", "--allow-empty", "-m", "noise"]);
	click("btn-apply");
	let stale_head =
		wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(stale_head.contains("stale_"), "{stale_head}");
	assert!(!dest_app.join("replay.txt").exists());
	git_ok(&dest_app, &["reset", "-q", "--soft", "HEAD~1"]);

	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	fs::write(dest_app.join("index-only.txt"), "idx\n").unwrap();
	git_ok(&dest_app, &["add", "index-only.txt"]);
	click("btn-apply");
	let stale_index =
		wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(stale_index.contains("stale_"), "{stale_index}");
	assert!(!dest_app.join("replay.txt").exists());
	git_ok(&dest_app, &["reset", "-q", "HEAD", "--", "index-only.txt"]);
	let _ = fs::remove_file(dest_app.join("index-only.txt"));

	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	capture_window(&wid, &out_dir.join("d3_replay_900x600.png"));
	send_key("Escape");
	wait_for("[APP:PASTE_CANCELLED]", Duration::from_secs(3));
	assert_eq!(git_rev(&dest_app), head_before);
	assert!(!dest_app.join("replay.txt").exists());

	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	click("paste-include:replay.txt");
	wait_for("[APP:PASTE_SEL_TOGGLED:", Duration::from_secs(3));
	click("btn-apply");
	let rejected =
		wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(rejected.contains("commit_subset_rejected"), "{rejected}");
	assert_eq!(git_rev(&dest_app), head_before);
	assert!(!dest_app.join("replay.txt").exists());

	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	click("btn-apply");
	wait_for("[APP:PASTE_DONE:", Duration::from_secs(8));
	assert_ne!(git_rev(&dest_app), head_before);
	let shown = Command::new("git")
		.current_dir(&dest_app)
		.args(["log", "-2", "--format=%an|%ae|%s"])
		.output()
		.unwrap();
	let shown = String::from_utf8(shown.stdout).unwrap();
	assert!(
		shown.contains("Alice|alice@example.com|tip replay"),
		"{shown}"
	);
	assert!(
		shown.contains("Alice|alice@example.com|root replay"),
		"{shown}"
	);
	let tip_body = Command::new("git")
		.current_dir(&dest_app)
		.args(["show", "HEAD:tip.txt"])
		.output()
		.unwrap();
	assert_eq!(tip_body.stdout, b"TIP_BYTES\n");
	let root_body = Command::new("git")
		.current_dir(&dest_app)
		.args(["show", "HEAD~1:replay.txt"])
		.output()
		.unwrap();
	assert_eq!(root_body.stdout, b"ROOT_BYTES\n");
	assert_eq!(
		fs::read_to_string(dest_app.join("keep.txt")).unwrap(),
		"KEEP_BYTES"
	);

	let head_replayed = git_rev(&dest_app);
	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	fs::write(dest_app.join("replay.txt"), "TAMPERED\n").unwrap();
	click("btn-apply");
	let stale_bytes =
		wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(stale_bytes.contains("stale_"), "{stale_bytes}");
	assert_eq!(git_rev(&dest_app), head_replayed);
	assert_eq!(
		fs::read_to_string(dest_app.join("replay.txt")).unwrap(),
		"TAMPERED\n"
	);

	quit_cleanly(&mut app, &wid);
}

/// Cross-repo file basket and a skipped non-UTF-8 replay that must not write.
#[test]
fn native_d3_files_and_replay_skip_oracles() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none());
		return;
	}
	if Command::new("xdotool").arg("--version").output().is_err() {
		assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none());
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let one = ws.path().join("repo-one");
	let two = ws.path().join("repo-two");
	let dest = ws.path().join("replay-dest");
	for dir in [&one, &two, &dest] {
		fs::create_dir_all(dir).unwrap();
		git_ok(dir, &["init", "-q", "-b", "main"]);
		git_ok(dir, &["config", "user.name", "Tester"]);
		git_ok(dir, &["config", "user.email", "test@example.com"]);
		git_ok(dir, &["config", "commit.gpgsign", "false"]);
	}
	fs::write(one.join("one.txt"), "ONE_BYTES\n").unwrap();
	git_ok(&one, &["add", "."]);
	git_ok(&one, &["commit", "-qm", "one"]);
	fs::write(two.join("two.txt"), "TWO_BYTES\n").unwrap();
	git_ok(&two, &["add", "."]);
	git_ok(&two, &["commit", "-qm", "two"]);
	fs::write(dest.join("a.txt"), "base\n").unwrap();
	git_ok(&dest, &["add", "."]);
	git_ok(&dest, &["commit", "-qm", "base"]);

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app =
		spawn_app(ws.path(), &dest, Some((bounds.clone(), viewport.clone())));
	let rx = &app.rx;
	let wait_for = |pattern: &str, timeout: Duration| -> String {
		lines_until(rx, pattern, timeout)
			.unwrap_or_else(|err| panic!("{err}"))
			.pop()
			.unwrap()
	};
	wait_for("[APP:READY_REPOS: 3]", Duration::from_secs(8));
	let wid = find_wid(app.pid);
	let send_key = |keys: &str| key(&wid, keys);
	let xdo = |args: &[&str]| {
		assert!(Command::new("xdotool")
			.args(args)
			.status()
			.unwrap()
			.success());
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(4);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0
						&& v[3] > 0 && v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} not drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		std::thread::sleep(Duration::from_millis(180));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let _ = Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status();
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"1",
		]);
	};
	xdo(&["windowsize", "--sync", &wid, "1080", "720"]);

	click("rail-project");
	wait_for("[APP:TAB_SWITCHED: FileExplorer", Duration::from_secs(3));
	click("btn-repo-selector");
	wait_for("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3));
	click("pick-repo:repo-one");
	wait_for("[APP:REPO_LOADED: repo-one", Duration::from_secs(6));
	click("tree-chk:one.txt");
	wait_for("[APP:BASKET: n=1", Duration::from_secs(3));
	click("btn-repo-selector");
	wait_for("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3));
	click("pick-repo:repo-two");
	wait_for("[APP:REPO_LOADED: repo-two", Duration::from_secs(6));
	click("tree-chk:two.txt");
	let basket = wait_for("[APP:BASKET: n=2", Duration::from_secs(3));
	assert!(basket.contains("one.txt"), "{basket}");
	assert!(basket.contains("two.txt"), "{basket}");
	click("rail-changes");
	wait_for("[APP:TAB_SWITCHED: GitChanges", Duration::from_secs(3));
	let out_dir = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out_dir).unwrap();
	capture_window(&wid, &out_dir.join("d3_basket_count_1080x720.png"));
	send_key("ctrl+c");
	wait_for("[APP:COPY_DONE: copied=2]", Duration::from_secs(6));
	wait_for("[APP:COPY_IDLE]", Duration::from_secs(3));
	let copied = clip::read_text().unwrap();
	assert!(copied.contains("ONE_BYTES\n"), "{copied}");
	assert!(copied.contains("TWO_BYTES\n"), "{copied}");

	click("btn-basket-clear");
	wait_for("[APP:BASKET_CLEARED]", Duration::from_secs(3));
	assert_eq!(
		clip::read_text().unwrap(),
		copied,
		"clearing the basket must not write the clipboard"
	);
	let sentinel = format!("SENTINEL_BASKET_{}", std::process::id());
	clip::write_text(&sentinel).unwrap();
	// The toolbar control is disabled for an empty basket. A click has no
	// handler, so it must not log a copy and must not replace the sentinel.
	click("btn-copy");
	match lines_until(rx, "[APP:COPY_DONE:", Duration::from_millis(500)) {
		Ok(lines) => panic!("disabled copy must not export: {lines:?}"),
		Err(err) => {
			let saw = err.split("saw ").nth(1).unwrap_or("");
			assert!(
				!saw.contains("COPY_DONE"),
				"disabled copy must not export: {err}"
			);
		}
	}
	assert_eq!(clip::read_text().unwrap(), sentinel);
	click("rail-changes");
	send_key("ctrl+c");
	let refusal = wait_for("[APP:COPY_REFUSED:", Duration::from_secs(3));
	assert!(
		refusal.contains("empty_selection")
			|| refusal.contains("commit_readonly"),
		"{refusal}"
	);
	assert_eq!(clip::read_text().unwrap(), sentinel);

	fs::write(dest.join("a.txt"), [0xff, 0xfe, 0x01]).unwrap();
	let head = git_rev(&dest);
	let index = {
		let out = Command::new("git")
			.current_dir(&dest)
			.args(["rev-parse", ":a.txt"])
			.output()
			.unwrap();
		assert!(out.status.success());
		String::from_utf8(out.stdout).unwrap()
	};
	let payload = commits::CommitsPayload {
		commits: vec![commits::CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![commits::CommitFile {
				path: "a.txt".into(),
				old_path: None,
				change: commits::FileChange::Modified,
				content: Some("incoming\n".into()),
				not_copied: None,
			}],
		}],
	};
	clip::write_text(&commits::to_clipboard_text(&payload)).unwrap();
	send_key("ctrl+v");
	wait_for("[APP:PASTE_PREVIEW:", Duration::from_secs(5));
	// Bounds arrive on the painted frame. Capturing on the log line alone
	// can still show the previous editor.
	control("paste-commit-whole");
	control("paste-row:a.txt");
	// The probe can land before the Vulkan frame is on the X pixmap.
	std::thread::sleep(Duration::from_millis(400));
	capture_window(&wid, &out_dir.join("d3_replay_skip_1080x720.png"));
	fs::write(dest.join("a.txt"), "external change\n").unwrap();
	click("btn-apply");
	let stale = wait_for("[APP:PASTE_STALE_DETECTED:", Duration::from_secs(5));
	assert!(stale.contains("stale_"), "{stale}");
	assert_eq!(
		fs::read_to_string(dest.join("a.txt")).unwrap(),
		"external change\n"
	);
	assert_eq!(git_rev(&dest), head);
	let index_after = Command::new("git")
		.current_dir(&dest)
		.args(["rev-parse", ":a.txt"])
		.output()
		.unwrap();
	assert_eq!(String::from_utf8(index_after.stdout).unwrap(), index);
	quit_cleanly(&mut app, &wid);
}

/// Root and nested Continue, retry, and selection at 900×600.
#[test]
fn native_tree_paging_retry_selection_900x600() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let xdotool = Command::new("xdotool").arg("--version").output();
	if xdotool.is_err() || !xdotool.unwrap().status.success() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	// Tests share one X display; overlapping windows steal clicks.
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("repo");
	fs::create_dir(&repo).unwrap();
	let git = |args: &[&str]| {
		let st = Command::new("git")
			.args(args)
			.current_dir(&repo)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.status()
			.unwrap();
		assert!(st.success(), "{args:?}");
	};
	git(&["init", "-q", "-b", "main"]);
	git(&["config", "user.email", "t@example.com"]);
	git(&["config", "user.name", "Tree Test"]);
	fs::write(repo.join("README.md"), "root\n").unwrap();
	git(&["add", "README.md"]);
	git(&["commit", "-qm", "init"]);
	for n in 0..130 {
		fs::write(repo.join(format!("f-{n:03}.txt")), b"x").unwrap();
	}
	let nest = repo.join("nest");
	fs::create_dir(&nest).unwrap();
	for n in 0..130 {
		fs::write(nest.join(format!("n-{n:03}.txt")), b"y").unwrap();
	}
	let broken = repo.join("broken");
	fs::create_dir(&broken).unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	let ready =
		lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(20))
			.expect("one repo must be discovered");
	assert!(
		ready
			.iter()
			.any(|line| line.contains("[APP:DISCOVERY_PROGRESS:")),
		"discovery must report progress before ready: {ready:?}"
	);
	let wid = find_wid(app.pid);
	let resize = |w: i32, h: i32| {
		let _ = Command::new("xdotool")
			.args(["windowmap", "--sync", &wid])
			.status();
		let st = Command::new("xdotool")
			.args([
				"windowsize",
				"--sync",
				&wid,
				&w.to_string(),
				&h.to_string(),
			])
			.status()
			.unwrap();
		assert!(st.success());
		let deadline = Instant::now() + Duration::from_secs(5);
		while *viewport.lock().unwrap() != (w, h) {
			assert!(
				Instant::now() < deadline,
				"viewport {:?}",
				*viewport.lock().unwrap()
			);
			std::thread::sleep(Duration::from_millis(50));
		}
	};
	// Tree pages land asynchronously and shift rows; acting on bounds from
	// an earlier frame hits the wrong row. Wait until the layout has held
	// still for a few frames.
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = settled();
			if let Some(v) = snap.get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(v[2] > 0 && v[3] > 0, "{id} {v:?}");
				assert!(
					v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} was not drawn");
			std::thread::sleep(Duration::from_millis(50));
		}
	};
	let click = |id: &str| {
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				"1",
			])
			.status()
			.unwrap();
		assert!(st.success(), "click {id}");
	};
	let has_id = |prefix: &str| -> Option<String> {
		settled().into_keys().find(|id| id.starts_with(prefix))
	};

	resize(900, 600);
	key(&wid, "alt+1");
	lines_until(
		&app.rx,
		"[APP:TAB_SWITCHED: FileExplorer",
		Duration::from_secs(5),
	)
	.expect("file explorer");
	let drawn = Instant::now() + Duration::from_secs(10);
	while has_id("tree-row:").is_none() && has_id("tree-continue:").is_none() {
		assert!(
			Instant::now() < drawn,
			"working tree did not draw: {:?}",
			bounds.lock().unwrap().keys().collect::<Vec<_>>()
		);
		std::thread::sleep(Duration::from_millis(50));
	}

	let deadline = Instant::now() + Duration::from_secs(8);
	while has_id("tree-row:nest").is_none()
		|| has_id("tree-row:broken").is_none()
	{
		if has_id("tree-continue:").is_some() {
			click("tree-continue:");
			let _ =
				lines_until(&app.rx, "[APP:TREE_PAGE:", Duration::from_secs(8));
		}
		assert!(
			Instant::now() < deadline,
			"nest/broken not drawn: {:?}",
			bounds.lock().unwrap().keys().collect::<Vec<_>>()
		);
		std::thread::sleep(Duration::from_millis(50));
	}
	assert!(has_id("tree-row:nest").is_some(), "nested directory row");
	assert!(has_id("tree-row:broken").is_some(), "broken directory row");

	// Topmost, not HashMap order: the bottom row can be clipped by the list.
	let chk = settled()
		.into_iter()
		.filter(|(id, _)| id.starts_with("tree-chk:f-"))
		.min_by_key(|(_, b)| b[1])
		.map(|(id, _)| id)
		.expect("a root file checkbox");
	click(&chk);
	let toggled =
		lines_until(&app.rx, "[APP:TREE_TOGGLED:", Duration::from_secs(5))
			.unwrap();
	let toggled = toggled.last().unwrap();
	assert!(toggled.contains("TREE_TOGGLED"));
	if has_id("tree-continue:").is_some() {
		click("tree-continue:");
		let page =
			lines_until(&app.rx, "[APP:TREE_PAGE:", Duration::from_secs(8))
				.unwrap();
		let page = page.last().unwrap();
		assert!(
			!page.contains("selected=0"),
			"selection must survive root Continue: {page}"
		);
	}

	click("tree-row:nest");
	lines_until(&app.rx, "[APP:TREE_EXPANDED: nest]", Duration::from_secs(8))
		.unwrap();
	click("tree-continue:nest");
	let nested = lines_until(
		&app.rx,
		"[APP:TREE_PAGE: rel=nest",
		Duration::from_secs(8),
	)
	.unwrap();
	let nested = nested.last().unwrap();
	assert!(
		!nested.contains("selected=0"),
		"selection must survive nested Continue: {nested}"
	);

	fs::remove_dir(&broken).unwrap();
	click("tree-row:broken");
	let deadline = Instant::now() + Duration::from_secs(8);
	while has_id("tree-retry:broken").is_none() {
		assert!(Instant::now() < deadline, "retry row for broken");
		std::thread::sleep(Duration::from_millis(50));
	}
	fs::create_dir(&broken).unwrap();
	fs::write(broken.join("repaired.txt"), b"ok").unwrap();
	click("tree-retry:broken");
	let deadline = Instant::now() + Duration::from_secs(8);
	while has_id("tree-row:broken/repaired.txt").is_none() {
		assert!(Instant::now() < deadline, "repaired file after retry");
		std::thread::sleep(Duration::from_millis(50));
	}

	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let png = out.join("tree_paging_900x600.png");
	let xwd = out.join("tree_paging_900x600.xwd");
	let st = Command::new("xwd")
		.args(["-id", &wid, "-silent", "-out", xwd.to_str().unwrap()])
		.status()
		.unwrap();
	assert!(st.success(), "xwd");
	let st = Command::new("convert")
		.args([xwd.to_str().unwrap(), png.to_str().unwrap()])
		.status()
		.unwrap();
	assert!(st.success(), "convert");
	assert!(fs::metadata(&png).unwrap().len() > 1024);
	let _ = fs::remove_file(&xwd);
	quit_cleanly(&mut app, &wid);
}

/// Real OS UI scenario selecting non-HEAD historical file into shared basket,
/// proving panel/repo switches, Space/checkbox toggle, collision rejection
/// with clipboard sentinel, successful multi-repo copy with exact git show bytes,
/// and basket clear without git mutations.
#[test]
fn native_historical_file_basket_and_collision() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let xdotool = Command::new("xdotool").arg("--version").output();
	if xdotool.is_err() || !xdotool.unwrap().status.success() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let ws_path = ws.path();
	let repo_a = ws_path.join("repo-a");
	let repo_b = ws_path.join("repo-b");
	let dest = tempfile::tempdir().unwrap();

	for dir in [&repo_a, &repo_b] {
		fs::create_dir_all(dir).unwrap();
		git_ok(dir, &["init", "-q", "-b", "main"]);
		git_ok(dir, &["config", "user.name", "Tester"]);
		git_ok(dir, &["config", "user.email", "test@example.com"]);
	}

	// repo-a: initial commit on main
	fs::write(repo_a.join("main.txt"), "main line\n").unwrap();
	git_ok(&repo_a, &["add", "main.txt"]);
	git_ok(&repo_a, &["commit", "-qm", "main commit"]);
	let main_sha = git_rev(&repo_a);

	// repo-a: unmerged feature commit with subfolder and feature_only.txt
	git_ok(&repo_a, &["checkout", "-q", "-b", "feature"]);
	fs::create_dir_all(repo_a.join("sub")).unwrap();
	fs::write(repo_a.join("sub").join("inner.txt"), "sub inner\n").unwrap();
	fs::write(
		repo_a.join("feature_only.txt"),
		"historical feature bytes\n",
	)
	.unwrap();
	git_ok(&repo_a, &["add", "."]);
	git_author_commit(&repo_a, "feature commit");
	let feat_sha = git_rev(&repo_a);
	let feat_short = feat_sha[..7].to_string();

	// repo-a: second feature branch with same path but different commit/content
	git_ok(&repo_a, &["checkout", "-q", "-b", "feature2", "main"]);
	fs::write(repo_a.join("feature_only.txt"), "feature2 distinct bytes\n")
		.unwrap();
	git_ok(&repo_a, &["add", "feature_only.txt"]);
	git_author_commit(&repo_a, "feature2 commit");
	let feat2_sha = git_rev(&repo_a);
	let feat2_short = feat2_sha[..7].to_string();

	// Switch repo-a back to main so both feature commits are non-HEAD / unmerged
	git_ok(&repo_a, &["checkout", "-q", "main"]);

	// Add an untracked file with the identical relative path to test collision policy
	fs::write(
		repo_a.join("feature_only.txt"),
		"untracked collision bytes\n",
	)
	.unwrap();

	// repo-b: initial commit and untracked b_file.txt
	fs::write(repo_b.join("init.txt"), "b init\n").unwrap();
	git_ok(&repo_b, &["add", "init.txt"]);
	git_ok(&repo_b, &["commit", "-qm", "b init"]);
	let b_sha = git_rev(&repo_b);
	fs::write(repo_b.join("b_file.txt"), "repo-b file bytes\n").unwrap();

	let out_dir = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out_dir).unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app = spawn_app(
		ws_path,
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	let rx = &app.rx;
	let wait_for = |pattern: &str, timeout: Duration| -> String {
		lines_until(rx, pattern, timeout)
			.unwrap_or_else(|e| panic!("{e}"))
			.pop()
			.unwrap()
	};
	wait_for("[APP:READY_REPOS: 2]", Duration::from_secs(12));
	wait_for("[APP:REPO_LOADED: repo-a", Duration::from_secs(6));
	wait_for("[APP:E2E_LOG:", Duration::from_secs(6));

	let wid = find_wid(app.pid);
	let activate = || {
		let _ = Command::new("xdotool")
			.args(["windowmap", "--sync", &wid])
			.status();
		let st = Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status()
			.expect("xdotool windowfocus must run");
		assert!(st.success(), "xdotool windowfocus failed for window {wid}");
		std::thread::sleep(Duration::from_millis(50));
	};
	activate();
	let send_key = |keys: &str| key(&wid, keys);
	let xdo = |args: &[&str]| {
		let st = Command::new("xdotool").args(args).status().unwrap();
		assert!(st.success(), "{args:?}");
	};
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(v[2] > 0 && v[3] > 0, "{id} empty {v:?}");
				assert!(
					v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} not drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		activate();
		std::thread::sleep(Duration::from_millis(150));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"1",
		]);
	};
	let resize = |w: i32, h: i32| {
		xdo(&["windowsize", "--sync", &wid, &w.to_string(), &h.to_string()]);
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if *viewport.lock().unwrap() == (w, h) {
				let snap = settled();
				if let (Some(sb), Some(bp), Some(bl)) = (
					snap.get("status-bar"),
					snap.get("btn-paste"),
					snap.get("btn-locale"),
				) {
					if sb[1] + sb[3] <= h
						&& bp[0] + bp[2] <= w
						&& bl[0] + bl[2] <= w
					{
						break;
					}
				}
			}
			assert!(
				Instant::now() < deadline,
				"viewport {w}x{h} did not settle in time"
			);
			std::thread::sleep(Duration::from_millis(40));
		}
	};

	let choose_repo = |id: &str, loaded: &str| {
		click("btn-repo-selector");
		wait_for("[APP:SELECTOR_OPEN: Repo", Duration::from_secs(3));
		click(id);
		wait_for(loaded, Duration::from_secs(6));
	};

	// 1. Initial 1080x720 layout and screenshot
	resize(1080, 720);

	// Select non-HEAD feature commit in GitLog
	click(&format!("commit-row:{feat_short}"));
	wait_for(
		&format!("[APP:COMMIT_SELECTED: {feat_short}]"),
		Duration::from_secs(6),
	);

	// Browse tree of historical commit
	wait_for(
		&format!("[APP:E2E_PREVIEW: source=commit_diff rev={feat_sha} "),
		Duration::from_secs(6),
	);
	click(&format!("btn-browse-tree:{feat_sha}"));
	wait_for(
		&format!("[APP:REV_TREE: {feat_short}]"),
		Duration::from_secs(6),
	);

	// Wait for historical tree controls to render
	let chk_probe = format!("rev-chk:{feat_sha}:feature_only.txt");
	control(&chk_probe);
	control("rev-row:feature_only.txt");
	control("rev-row:sub");

	// Directory must NOT have a checkbox probe
	assert!(
		bounds.lock().unwrap().get(&format!("rev-chk:{feat_sha}:sub")).is_none(),
		"directory row must not masquerade as selectable file with a checkbox probe"
	);

	// Capture unselected 1080x720 screenshot
	settled();
	std::thread::sleep(Duration::from_millis(200));
	let shot_1080 = out_dir.join("d3_historical_basket_1080x720.png");
	capture_window(&wid, &shot_1080);

	// Capture unselected 900x600 screenshot (settled layout, key controls bounded)
	resize(900, 600);
	std::thread::sleep(Duration::from_millis(200));
	let shot_900 = out_dir.join("d3_historical_basket_900x600.png");
	capture_window(&wid, &shot_900);

	// Restore 1080x720
	resize(1080, 720);

	// Test Space toggle and row navigation on historical file:
	// Clicking row only selects for preview
	click("rev-row:feature_only.txt");
	wait_for(
		"[APP:PREVIEW_LOADED: feature_only.txt]",
		Duration::from_secs(6),
	);
	// Clicking directory row navigates/expands and must NOT affect basket
	click("rev-row:sub");
	click("rev-row:feature_only.txt");
	assert!(bounds.lock().unwrap().get("basket-summary").is_some());

	// Press Space to toggle selection on
	send_key("space");
	wait_for(
		&format!("[APP:REV_FILE_TOGGLED: sha={feat_short} path=feature_only.txt selected=true]"),
		Duration::from_secs(4),
	);
	let b_line = wait_for("[APP:BASKET: n=1", Duration::from_secs(4));
	assert!(
		b_line.contains(&format!("commit@{feat_short}"))
			|| b_line.contains("commit@")
	);
	assert!(b_line.contains("feature_only.txt"));

	// Navigation while selected must NOT change basket
	click("rev-row:sub");
	click("rev-row:feature_only.txt");
	let sum_v = control("basket-summary");
	assert!(sum_v[2] > 0);

	// Press Space again to toggle selection off
	send_key("space");
	wait_for(
		&format!("[APP:REV_FILE_TOGGLED: sha={feat_short} path=feature_only.txt selected=false]"),
		Duration::from_secs(4),
	);
	wait_for("[APP:BASKET: n=0", Duration::from_secs(4));

	// Click checkbox directly to toggle selection on
	click(&chk_probe);
	wait_for(
		&format!("[APP:REV_FILE_TOGGLED: sha={feat_short} path=feature_only.txt selected=true]"),
		Duration::from_secs(4),
	);
	let b_line = wait_for("[APP:BASKET: n=1", Duration::from_secs(4));
	assert!(b_line.contains("feature_only.txt"));

	// Capture selected screenshots at 1080x720 and 900x600
	settled();
	std::thread::sleep(Duration::from_millis(200));
	let shot_1080_sel =
		out_dir.join("d3_historical_basket_1080x720_selected.png");
	capture_window(&wid, &shot_1080_sel);

	resize(900, 600);
	std::thread::sleep(Duration::from_millis(200));
	let shot_900_sel =
		out_dir.join("d3_historical_basket_900x600_selected.png");
	capture_window(&wid, &shot_900_sel);

	resize(1080, 720);

	// Test exact full-OID identity deselection with TWO revisions of the same relative path:
	click("btn-leave-tree");
	wait_for("[APP:REV_TREE: off]", Duration::from_secs(4));

	// Select feat2 in GitLog and browse its tree
	click(&format!("commit-row:{feat2_short}"));
	wait_for(
		&format!("[APP:COMMIT_SELECTED: {feat2_short}]"),
		Duration::from_secs(6),
	);
	wait_for(
		&format!("[APP:E2E_PREVIEW: source=commit_diff rev={feat2_sha} "),
		Duration::from_secs(6),
	);
	click(&format!("btn-browse-tree:{feat2_sha}"));
	wait_for(
		&format!("[APP:REV_TREE: {feat2_short}]"),
		Duration::from_secs(6),
	);

	let chk_probe_feat2 = format!("rev-chk:{feat2_sha}:feature_only.txt");
	control(&chk_probe_feat2);

	// Select feat2 revision of feature_only.txt: basket now holds 2 revisions of same path
	click(&chk_probe_feat2);
	wait_for(
		&format!("[APP:REV_FILE_TOGGLED: sha={feat2_short} path=feature_only.txt selected=true]"),
		Duration::from_secs(4),
	);
	let b_line = wait_for("[APP:BASKET: n=2", Duration::from_secs(4));
	assert!(b_line.contains(&format!("commit@{feat_short}")));
	assert!(b_line.contains(&format!("commit@{feat2_short}")));

	// Deselect feat2 revision: ONLY feat2 is removed, feat1 remains in basket!
	click(&chk_probe_feat2);
	wait_for(
		&format!("[APP:REV_FILE_TOGGLED: sha={feat2_short} path=feature_only.txt selected=false]"),
		Duration::from_secs(4),
	);
	let b_line = wait_for("[APP:BASKET: n=1", Duration::from_secs(4));
	assert!(
		b_line.contains(&format!("commit@{feat_short}")),
		"feat1 selection must remain after feat2 deselected"
	);
	assert!(
		!b_line.contains(&format!("commit@{feat2_short}")),
		"feat2 selection must be gone"
	);

	click("btn-leave-tree");
	wait_for("[APP:REV_TREE: off]", Duration::from_secs(4));

	// Switch to Git Changes tab
	click("rail-changes");
	wait_for("[APP:TAB_SWITCHED: GitChanges", Duration::from_secs(4));

	// Switch to repo-b
	choose_repo("pick-repo:repo-b", "[APP:REPO_LOADED: repo-b");

	// In repo-b, select b_file.txt from GitChanges
	click("change-chk:b_file.txt");
	wait_for("[APP:BASKET: n=2", Duration::from_secs(4));

	// Switch back to repo-a
	choose_repo("pick-repo:repo-a", "[APP:REPO_LOADED: repo-a");

	// Basket must still hold 2 items!
	let summary_b = control("basket-summary");
	assert!(summary_b[2] > 0);

	// 3. Collision refusal with clipboard sentinel
	let sentinel = "SENTINEL_HISTORICAL_COLLISION_MUST_NOT_OVERWRITE";
	clip_set(sentinel);
	assert_eq!(clip_get(), sentinel);

	// In repo-a, select untracked feature_only.txt (which collides with commit@feat_sha:feature_only.txt)
	click("change-chk:feature_only.txt");
	let col_line = wait_for("[APP:BASKET_COLLISION:", Duration::from_secs(4));
	assert!(col_line.contains("feature_only.txt"));

	// Click Copy button: must refuse due to collision!
	click("btn-copy");
	wait_for("[APP:COPY_REFUSED: collision]", Duration::from_secs(4));

	// Clipboard MUST remain the unchanged sentinel
	assert_eq!(
		clip_get(),
		sentinel,
		"clipboard must not be overwritten when copy is refused due to collision"
	);

	// 4. Deselect colliding file, then successful copy
	click("change-chk:feature_only.txt");
	wait_for("[APP:BASKET: n=2", Duration::from_secs(4));

	// Copy now succeeds!
	click("btn-copy");
	wait_for("[APP:COPY_PREP: files=2]", Duration::from_secs(4));
	wait_for("[APP:COPY_DONE: copied=2]", Duration::from_secs(8));

	// Verify clipboard payload against git show exact bytes
	let copied_text = clip_get();
	assert_ne!(copied_text, sentinel);

	let parsed = snip_core::format::parse_clipboard(&copied_text, "");
	assert_eq!(parsed.len(), 2, "copied payload must contain 2 entries");

	let feat_entry = parsed
		.iter()
		.find(|e| e.path.ends_with("feature_only.txt"))
		.expect("feature_only.txt in clipboard payload");
	let git_show = Command::new("git")
		.current_dir(&repo_a)
		.args(["show", &format!("{feat_sha}:feature_only.txt")])
		.output()
		.expect("git show");
	assert!(git_show.status.success());
	let expected_feat_bytes = String::from_utf8(git_show.stdout).unwrap();
	assert_eq!(
		feat_entry.content,
		expected_feat_bytes.trim_end_matches('\n'),
		"historical file content must match git show normalized by codec"
	);

	let b_entry = parsed
		.iter()
		.find(|e| e.path.ends_with("b_file.txt"))
		.expect("b_file.txt in clipboard payload");
	assert_eq!(
		b_entry.content, "repo-b file bytes",
		"repo-b file content must match working file"
	);

	// 5. Clear basket
	click("btn-basket-clear");
	wait_for("[APP:BASKET: n=0", Duration::from_secs(4));
	wait_for("[APP:BASKET_CLEARED]", Duration::from_secs(4));

	// Re-verify in rev tree that checkbox is cleared
	click(&format!("commit-row:{feat_short}"));
	wait_for(
		&format!("[APP:COMMIT_SELECTED: {feat_short}]"),
		Duration::from_secs(6),
	);
	wait_for(
		&format!("[APP:E2E_PREVIEW: source=commit_diff rev={feat_sha} "),
		Duration::from_secs(6),
	);
	click(&format!("btn-browse-tree:{feat_sha}"));
	wait_for(
		&format!("[APP:REV_TREE: {feat_short}]"),
		Duration::from_secs(6),
	);
	control(&chk_probe);

	// 6. Verify NO HEAD / index / worktree mutations in either repository
	let st_a = Command::new("git")
		.current_dir(&repo_a)
		.args(["status", "--porcelain"])
		.output()
		.unwrap();
	assert_eq!(
		String::from_utf8(st_a.stdout).unwrap().trim(),
		"?? feature_only.txt",
		"repo-a must have no unexpected mutations"
	);
	assert_eq!(
		git_rev(&repo_a),
		main_sha,
		"repo-a must remain on main commit"
	);

	let st_b = Command::new("git")
		.current_dir(&repo_b)
		.args(["status", "--porcelain"])
		.output()
		.unwrap();
	assert_eq!(
		String::from_utf8(st_b.stdout).unwrap().trim(),
		"?? b_file.txt",
		"repo-b must have no unexpected mutations"
	);
	assert_eq!(
		git_rev(&repo_b),
		b_sha,
		"repo-b must remain on initial commit"
	);

	quit_cleanly(&mut app, &wid);
}

/// Error bodies must never copy hidden old text. Display clipping must not
/// replace the retained source bytes used by Copy View or reader selection.
#[test]
fn native_reader_degradation_and_copy_integrity() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();
	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("reader");
	fs::create_dir(&repo).unwrap();
	git_ok(&repo, &["init", "-q", "-b", "main"]);
	git_ok(&repo, &["config", "user.name", "Reader Test"]);
	git_ok(&repo, &["config", "user.email", "reader@example.com"]);
	let good = "\u{feff}  exact source\t \r\n第二行\r\n\r\n";
	let mut selection_mismatches = Vec::new();
	let long = format!(
		"{}繁體中文𝄞{}HIDDEN-END",
		"a".repeat(4091),
		"b".repeat(12_000)
	);
	let many = "row\n".repeat(50_001);
	let files: Vec<(&str, Vec<u8>)> = vec![
		("good.txt", good.as_bytes().to_vec()),
		("long.txt", long.as_bytes().to_vec()),
		("many-lines.txt", many.as_bytes().to_vec()),
		("binary.bin", b"binary\0bytes".to_vec()),
		("not-utf8.txt", vec![b'x', 0xff, b'y']),
		("large.txt", vec![b'L'; 1024 * 1024 + 1]),
	];
	for (name, bytes) in &files {
		fs::write(repo.join(name), bytes).unwrap();
	}
	git_ok(&repo, &["add", "."]);
	git_ok(&repo, &["commit", "-qm", "reader fixtures"]);
	let head = git_rev(&repo);
	let index_before = fs::read(repo.join(".git/index")).unwrap();
	let git_good = Command::new("git")
		.current_dir(&repo)
		.args(["show", &format!("{head}:good.txt")])
		.output()
		.unwrap();
	assert!(git_good.status.success());
	assert_eq!(git_good.stdout, good.as_bytes());
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let dest = tempfile::tempdir().unwrap();
	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app = spawn_app(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	let wait = |pattern: &str| {
		lines_until(&app.rx, pattern, Duration::from_secs(8))
			.unwrap_or_else(|error| panic!("{error}"))
	};
	wait("[APP:READY_REPOS: 1]");
	wait("[APP:REPO_LOADED: reader");
	wait("[APP:E2E_LOG:");
	let wid = find_wid(app.pid);
	// Map and focus the window on private X11 without a window manager.
	key(&wid, "Escape");
	assert!(Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "900", "600"])
		.status()
		.unwrap()
		.success());
	wait("[APP:VIEWPORT: 900x600]");
	assert_eq!(*viewport.lock().unwrap(), (900, 600));
	let capture = |path: &Path| {
		let geometry = Command::new("xdotool")
			.args(["getwindowgeometry", "--shell", &wid])
			.output()
			.unwrap();
		assert!(geometry.status.success());
		let text = String::from_utf8(geometry.stdout).unwrap();
		let geometry: HashMap<_, _> = text
			.lines()
			.filter_map(|line| line.split_once('='))
			.collect();
		let crop = format!(
			"{}x{}+{}+{}",
			geometry["WIDTH"], geometry["HEIGHT"], geometry["X"], geometry["Y"]
		);
		let xwd = tempfile::Builder::new().suffix(".xwd").tempfile().unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			assert!(Command::new("xwd")
				.args(["-root", "-silent", "-out"])
				.arg(xwd.path())
				.status()
				.unwrap()
				.success());
			assert!(Command::new("convert")
				.arg(xwd.path())
				.args(["-crop", &crop, "+repage"])
				.arg(path)
				.status()
				.unwrap()
				.success());
			if fs::metadata(path).unwrap().len() > 1024 {
				break;
			}
			assert!(
				Instant::now() < deadline,
				"window must present a nonblank frame"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			let v = bounds.lock().unwrap().get(id).copied();
			if let Some(v) = v {
				let (w, h) = *viewport.lock().unwrap();
				// A viewport event precedes the next frame's control bounds.
				if v[2] > 0
					&& v[3] > 0 && v[0] >= 0
					&& v[1] >= 0 && v[0] + v[2] <= w
					&& v[1] + v[3] <= h
				{
					return v;
				}
			}
			if Instant::now() >= deadline {
				capture(&out.join("reader-missing-control.png"));
				fs::write(
					out.join("reader-missing-control-bounds.txt"),
					format!(
						"missing or outside viewport={id}\n{:?}",
						bounds.lock().unwrap()
					),
				)
				.unwrap();
				panic!("{id} not drawn fully inside viewport");
			}
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(5);
		let v = loop {
			let before = control(id);
			std::thread::sleep(Duration::from_millis(150));
			let after = control(id);
			if before == after {
				break after;
			}
			assert!(Instant::now() < deadline, "{id} bounds did not settle");
		};
		assert!(Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status()
			.unwrap()
			.success());
		assert!(Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&(v[0] + v[2] / 2).to_string(),
				&(v[1] + v[3] / 2).to_string(),
				"click",
				"1"
			])
			.status()
			.unwrap()
			.success());
	};
	let open = |prefix: &str, path: &str| {
		click(&format!("{prefix}:{path}"));
		wait(&format!("[APP:TREE_FILE_SELECTED: {path}]"));
		wait(&format!("[APP:PREVIEW_LOADED: {path}]"));
		control("reader");
	};
	let assert_no_copy = |sentinel: &str, evidence: &str| {
		let deadline = Instant::now() + Duration::from_millis(400);
		let mut events = Vec::new();
		while Instant::now() < deadline {
			match app.rx.recv_timeout(Duration::from_millis(40)) {
				Ok(line) => events.push(line),
				Err(RecvTimeoutError::Timeout) => {}
				Err(RecvTimeoutError::Disconnected) => {
					panic!("app exited during refusal")
				}
			}
		}
		let actual = clip_get();
		fs::write(out.join(format!("{evidence}-clipboard.txt")), &actual)
			.unwrap();
		fs::write(
			out.join(format!("{evidence}-events.txt")),
			events.join("\n"),
		)
		.unwrap();
		capture(&out.join(format!("{evidence}.png")));
		// Keep this assertion first: the old product must fail on actual stale
		// clipboard bytes, before any newly added notice probe is needed.
		assert_eq!(
			actual, sentinel,
			"{evidence}: error body copied hidden old text"
		);
		assert!(
			!events
				.iter()
				.any(|line| line.contains("[APP:PREVIEW_COPIED:")
					|| line.contains("[APP:SELECTION_COPIED:")
					|| line.contains("[APP:COPY_DONE:")),
			"{evidence}: unexpected copy event: {events:?}"
		);
	};

	click(&format!("commit-row:{}", &head[..7]));
	wait(&format!("[APP:COMMIT_SELECTED: {}]", &head[..7]));
	wait(&format!("[APP:E2E_PREVIEW: source=commit_diff rev={head} "));
	click(&format!("btn-browse-tree:{head}"));
	wait(&format!("[APP:REV_TREE: {}]", &head[..7]));
	for path in ["binary.bin", "not-utf8.txt", "large.txt"] {
		open("rev-row", "good.txt");
		click("reader");
		key(&wid, "ctrl+a");
		key(&wid, "ctrl+c");
		wait("[APP:SELECTION_COPIED:");
		assert!(
			!clip_get().is_empty(),
			"old selection must exist before failure"
		);
		let sentinel = format!("reader refusal sentinel: {path}\r\n  ");
		clip_set(&sentinel);
		click(&format!("rev-row:{path}"));
		wait(&format!("[APP:TREE_FILE_SELECTED: {path}]"));
		control("editor-error");
		std::thread::sleep(Duration::from_millis(100));
		capture_window(
			&wid,
			&out.join(format!("reader-{path}-before-copy.png")),
		);
		click("btn-copy-view");
		assert_no_copy(&sentinel, &format!("reader-{path}-refusal"));
	}

	open("rev-row", "good.txt");
	click("btn-copy-view");
	wait("[APP:PREVIEW_COPIED:");
	assert_eq!(
		clip_get().as_bytes(),
		git_good.stdout,
		"BOM, CRLF, spaces and final newlines survive Copy View"
	);
	click("reader");
	key(&wid, "ctrl+a");
	key(&wid, "ctrl+c");
	wait("[APP:SELECTION_COPIED:");
	let selected = clip_get();
	fs::write(out.join("reader-good-selection.txt"), &selected).unwrap();
	capture(&out.join("reader-good-selection.png"));
	if selected.as_bytes() != git_good.stdout {
		selection_mismatches.push(format!(
			"good.txt Ctrl+A differs from git show: expected {} bytes, copied {} bytes",
			git_good.stdout.len(), selected.len()));
	}

	open("rev-row", "long.txt");
	std::thread::sleep(Duration::from_millis(150));
	capture(&out.join("reader-long-zh.png"));
	let notice = control("reader-truncated-notice");
	let reader = control("reader");
	assert!(
		notice[0] >= reader[0]
			&& notice[1] >= reader[1]
			&& notice[0] + notice[2] <= reader[0] + reader[2]
			&& notice[1] + notice[3] <= reader[1] + reader[3],
		"line notice {notice:?} must be visible inside reader {reader:?}"
	);
	let row = bounds.lock().unwrap().get("code-text:1").copied().unwrap();
	fs::write(out.join("reader-long-layout.txt"),
		format!("viewport={:?}\nnotice={notice:?}\nreader={reader:?}\nrow={row:?}\n", *viewport.lock().unwrap())).unwrap();
	assert!(row[3] >= 18 && row[1] >= notice[1] + notice[3]
		&& row[1] + row[3] <= reader[1] + reader[3]
		&& row[0] < reader[0] + reader[2] && row[0] + row[2] > reader[0],
		"notice must leave a full actual text row: row={row:?}, reader={reader:?}");
	control("log-list"); // The Git panel stays open throughout this proof.
	click("btn-copy-view");
	wait("[APP:PREVIEW_COPIED:");
	assert_eq!(
		clip_get(),
		long,
		"display clipping must not clip whole preview copy"
	);

	click("reader");
	key(&wid, "ctrl+a");
	key(&wid, "ctrl+c");
	wait("[APP:SELECTION_COPIED:");
	assert_eq!(
		clip_get(),
		long,
		"Select All retains the hidden suffix, with no display marker"
	);
	// The locale event precedes paint. Compare only the warning rectangle,
	// excluding the reader selection, pointer, and other changing controls.
	let notice_pixels = |image: &Path, crop: &Path| {
		let [x, y, w, h] = notice;
		let pixels = Command::new("convert")
			.arg(image)
			.args([
				"-crop",
				&format!("{w}x{h}+{x}+{y}"),
				"+repage",
				"-depth",
				"8",
				"-write",
			])
			.arg(crop)
			.arg("RGB:-")
			.output()
			.unwrap();
		assert!(
			pixels.status.success(),
			"notice crop failed: {:?}",
			pixels.stderr
		);
		assert_eq!(pixels.stdout.len(), (w * h * 3) as usize);
		pixels.stdout
	};
	// Text antialiasing blends warning ink with its dark background.
	let has_warning_ink = |pixels: &[u8]| {
		pixels.as_chunks::<3>().0.iter().any(|p| {
			// DARK.warning #d59637.
			p[0].abs_diff(213) <= 16
				&& p[1].abs_diff(150) <= 16
				&& p[2].abs_diff(55) <= 16
		})
	};
	let deadline = Instant::now() + Duration::from_secs(5);
	let mut previous = Vec::new();
	let chinese_pixels = loop {
		capture(&out.join("reader-long-zh.png"));
		let pixels = notice_pixels(
			&out.join("reader-long-zh.png"),
			&out.join("reader-long-zh-notice.png"),
		);
		if has_warning_ink(&pixels) && pixels == previous {
			break pixels;
		}
		assert!(
			Instant::now() < deadline,
			"Chinese notice did not paint and settle"
		);
		previous = pixels;
		std::thread::sleep(Duration::from_millis(100));
	};
	click("btn-locale");
	wait("[APP:LOCALE: En]");
	assert_eq!(control("reader-truncated-notice"), notice);
	let deadline = Instant::now() + Duration::from_secs(5);
	let mut previous = Vec::new();
	loop {
		capture(&out.join("reader-long-en.png"));
		let pixels = notice_pixels(
			&out.join("reader-long-en.png"),
			&out.join("reader-long-en-notice.png"),
		);
		if has_warning_ink(&pixels)
			&& pixels != chinese_pixels
			&& pixels == previous
		{
			break;
		}
		assert!(
			Instant::now() < deadline,
			"English notice did not paint and settle"
		);
		previous = pixels;
		std::thread::sleep(Duration::from_millis(100));
	}

	open("rev-row", "many-lines.txt");
	control("reader-truncated-notice");
	capture(&out.join("reader-many-lines.png"));
	click("btn-copy-view");
	wait("[APP:PREVIEW_COPIED:");
	assert_eq!(
		clip_get(),
		many,
		"line indexing cap must not discard retained source bytes"
	);
	click("reader");
	key(&wid, "ctrl+a");
	key(&wid, "ctrl+c");
	wait("[APP:SELECTION_COPIED:");
	let selected = clip_get();
	fs::write(out.join("reader-many-lines-selection.txt"), &selected).unwrap();
	capture(&out.join("reader-many-lines-selection.png"));
	if selected != many {
		selection_mismatches.push(format!(
			"many-lines.txt Ctrl+A omitted retained source: expected {} bytes, copied {} bytes",
			many.len(), selected.len()));
	}

	click("btn-leave-tree");
	wait("[APP:REV_TREE: off]");
	open("tree-row", "good.txt");
	// Actual filesystem read failure, with a still-rendered tree row.
	fs::remove_file(repo.join("good.txt")).unwrap();
	let sentinel = "working read failure sentinel\n";
	clip_set(sentinel);
	click("tree-row:good.txt");
	wait("[APP:TREE_FILE_SELECTED: good.txt]");
	control("editor-error");
	click("btn-copy-view");
	assert_no_copy(sentinel, "reader-missing-working-file");
	fs::write(repo.join("good.txt"), good).unwrap();
	open("tree-row", "good.txt");
	click("btn-copy-view");
	wait("[APP:PREVIEW_COPIED:");
	assert_eq!(
		clip_get(),
		good,
		"valid read recovers raw copy after an error"
	);
	assert_eq!(git_rev(&repo), head);
	assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index_before);
	for (name, bytes) in &files {
		assert_eq!(
			&fs::read(repo.join(name)).unwrap(),
			bytes,
			"{name} changed"
		);
	}
	quit_cleanly(&mut app, &wid);
	// Preserve both actual clipboard mismatches in an old-product run.
	assert!(selection_mismatches.is_empty(), "{selection_mismatches:?}");
}

/// `SNIP_THEME=light` renders the Islands Light palette. Saves
/// `light_theme.png` as an artifact; the only pixel check is that the
/// window is predominantly light (the dark palette averages well below).
#[test]
fn native_light_theme_renders() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();
	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("light");
	fs::create_dir(&repo).unwrap();
	git_ok(&repo, &["init", "-q", "-b", "main"]);
	git_ok(&repo, &["config", "user.name", "Light Test"]);
	git_ok(&repo, &["config", "user.email", "light@example.com"]);
	fs::write(repo.join("README.md"), "light 淺色\n").unwrap();
	git_ok(&repo, &["add", "."]);
	git_ok(&repo, &["commit", "-q", "-m", "initial"]);
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app_themed(ws.path(), dest.path(), None, "light");
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(8))
		.unwrap();
	let wid = find_wid(app.pid);
	// Map and focus the window so it presents a frame (as the graph test does).
	key(&wid, "Escape");
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let png = out.join("light_theme.png");
	// Vulkan presents to the root surface under Xvfb; crop the window from it.
	let geometry = Command::new("xdotool")
		.args(["getwindowgeometry", "--shell", &wid])
		.output()
		.unwrap();
	assert!(geometry.status.success());
	let text = String::from_utf8(geometry.stdout).unwrap();
	let geometry: HashMap<_, _> = text
		.lines()
		.filter_map(|line| line.split_once('='))
		.collect();
	let crop = format!(
		"{}x{}+{}+{}",
		geometry["WIDTH"], geometry["HEIGHT"], geometry["X"], geometry["Y"]
	);
	let xwd = tempfile::Builder::new().suffix(".xwd").tempfile().unwrap();
	let deadline = Instant::now() + Duration::from_secs(10);
	let mean = loop {
		assert!(Command::new("xwd")
			.args(["-root", "-silent", "-out"])
			.arg(xwd.path())
			.status()
			.unwrap()
			.success());
		assert!(Command::new("convert")
			.arg(xwd.path())
			.args(["-crop", &crop, "+repage"])
			.arg(&png)
			.status()
			.unwrap()
			.success());
		let stat = Command::new("convert")
			.arg(&png)
			.args(["-colorspace", "Gray", "-format", "%[fx:mean]", "info:"])
			.output()
			.unwrap();
		assert!(stat.status.success());
		let mean: f64 = String::from_utf8(stat.stdout)
			.unwrap()
			.trim()
			.parse()
			.unwrap();
		if mean > 0.6 || Instant::now() >= deadline {
			break mean;
		}
		std::thread::sleep(Duration::from_millis(100));
	};
	println!("[TEST DRIVER] light theme mean luminance {mean:.3} -> {png:?}");
	assert!(mean > 0.6, "light palette must render light, mean={mean}");
	quit_cleanly(&mut app, &wid);
}

/// IntelliJ chrome through real input: a right-click context menu run by
/// mouse and by keyboard, speed search in the Project tree, Esc / Shift+Esc,
/// Log ← to the parent commit and Ctrl+Shift+` for the branches popup.
#[test]
fn native_intellij_menus_shortcuts_and_speed_search() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let xdotool = Command::new("xdotool").arg("--version").output();
	if xdotool.is_err() || !xdotool.unwrap().status.success() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("chrome");
	fs::create_dir_all(repo.join("subfolder")).unwrap();
	git_ok(&repo, &["init", "-q", "-b", "main"]);
	git_ok(&repo, &["config", "user.email", "t@example.com"]);
	git_ok(&repo, &["config", "user.name", "Chrome Test"]);
	fs::write(repo.join("alpha.txt"), "alpha\n").unwrap();
	fs::write(repo.join("subfolder/nested.txt"), "nested\n").unwrap();
	git_ok(&repo, &["add", "."]);
	git_ok(&repo, &["commit", "-qm", "first"]);
	let first = git_rev(&repo);
	fs::write(repo.join("alpha.txt"), "alpha two\n").unwrap();
	git_ok(&repo, &["commit", "-qam", "second"]);
	let second = git_rev(&repo);
	fs::write(repo.join("gamma.txt"), "untracked\n").unwrap();
	fs::write(repo.join("delta.txt"), "untracked\n").unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(12))
		.expect("repo must load");
	lines_until(&app.rx, "[APP:E2E_LOG:", Duration::from_secs(8))
		.expect("log must load");
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	let st = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status()
		.unwrap();
	assert!(st.success());
	let rx = &app.rx;
	let wait = |pattern: &str| {
		lines_until(rx, pattern, Duration::from_secs(5))
			.unwrap_or_else(|e| panic!("{pattern}: {e}"))
	};
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if let Some(v) = settled().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0
						&& v[3] > 0 && v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} was not drawn");
		}
	};
	let absent = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(4);
		while bounds.lock().unwrap().contains_key(id) {
			assert!(Instant::now() < deadline, "{id} must not be drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let press = |id: &str, button: &str| {
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				button,
			])
			.status()
			.unwrap();
		assert!(st.success());
	};
	let click = |id: &str| press(id, "1");
	let right_click = |id: &str| press(id, "3");

	key(&wid, "alt+1");
	wait("[APP:TAB_SWITCHED: FileExplorer visible=true");
	click("tree-row:subfolder");
	wait("[APP:TREE_EXPANDED: subfolder]");

	// 1. Mouse: right-click a file, pick "Copy Relative Path".
	clip_set("SENTINEL_CHROME_MENU");
	right_click("tree-row:subfolder/nested.txt");
	let open = wait("[APP:MENU_OPEN: Left");
	assert!(
		open.last().unwrap().contains("copy-relative-path")
			&& open.last().unwrap().contains("reveal"),
		"{open:?}"
	);
	control("context-menu");
	click("menu-item:copy-relative-path");
	wait("[APP:MENU_ACTION: copy-relative-path]");
	wait("[APP:TEXT_COPIED:");
	absent("context-menu");
	assert_eq!(clip_get(), "subfolder/nested.txt");

	// 2. Keyboard: Down highlights the first item, Enter runs it.
	right_click("tree-row:alpha.txt");
	wait("[APP:MENU_OPEN: Left items=add-basket");
	key(&wid, "Down");
	key(&wid, "Return");
	wait("[APP:MENU_ACTION: add-basket]");
	wait("[APP:BASKET: n=1");
	absent("context-menu");
	// Escape closes a menu without running anything.
	right_click("tree-row:alpha.txt");
	wait("[APP:MENU_OPEN: Left items=remove-basket");
	key(&wid, "Escape");
	let closed = wait("[APP:MENU_CLOSED]");
	assert!(
		!closed.iter().any(|l| l.contains("MENU_ACTION")),
		"{closed:?}"
	);

	// 3. Speed search: typing in the tree jumps to the first match.
	click("tree-row:alpha.txt");
	Command::new("xdotool")
		.args(["type", "--window", &wid, "nest"])
		.status()
		.unwrap();
	let found = wait("[APP:SPEED_SEARCH: q=nest row=");
	assert!(!found.last().unwrap().contains("row=none"), "{found:?}");
	control("speed-search");
	key(&wid, "Escape");
	wait("[APP:SPEED_SEARCH: off]");
	absent("speed-search");
	// A second Esc leaves the tool window for the editor.
	key(&wid, "Escape");
	wait("[APP:FOCUS: editor]");

	// 3b. Editor tab menu: right-click the tab strip (just above the
	// breadcrumb bar) and Close the tab.
	let crumb = control("breadcrumb");
	let (x, y) = (crumb[0] + 30, crumb[1] - 16);
	Command::new("xdotool")
		.args([
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"3",
		])
		.status()
		.unwrap();
	let open = wait("[APP:MENU_OPEN: Editor");
	assert!(
		open.last().unwrap().contains("close-tab,close-other-tabs"),
		"{open:?}"
	);
	click("menu-item:close-tab");
	wait("[APP:TAB_CLOSED: 0]");
	absent("context-menu");

	// 4. Shift+Esc hides the focused tool window.
	click("tree-row:alpha.txt");
	key(&wid, "shift+Escape");
	wait("[APP:TAB_SWITCHED: FileExplorer visible=false");
	absent("left-list");

	// 5. Log: Left goes to the parent commit.
	click(&format!("commit-row:{}", &second[..7]));
	wait(&format!("[APP:COMMIT_SELECTED: {}]", &second[..7]));
	key(&wid, "Left");
	wait(&format!("[APP:COMMIT_SELECTED: {}]", &first[..7]));
	// Right-click menu on a Log row offers the parent as well.
	right_click(&format!("commit-row:{}", &second[..7]));
	wait("[APP:MENU_OPEN: Log");
	click("menu-item:copy-revision");
	wait("[APP:MENU_ACTION: copy-revision]");
	wait("[APP:TEXT_COPIED:");
	assert_eq!(clip_get(), second);
	// 5b. The changed-files pane: right-click a file of that commit and
	// copy it (snip-sync payload) as it was in the commit.
	clip_set("SENTINEL_LOG_FILE_MENU");
	right_click("commit-file:alpha.txt");
	wait("[APP:MENU_OPEN: Log items=copy-files,");
	click("menu-item:copy-files");
	wait("[APP:MENU_ACTION: copy-files]");
	wait("[APP:COPY_DONE: copied=1]");
	absent("context-menu");
	// A card over the window says so, not only the status bar.
	wait("[APP:TOAST: ok=true]");
	control("copy-toast");
	let copied = clip_get();
	assert!(
		copied.contains("alpha.txt") && copied.contains("alpha two\n"),
		"{copied}"
	);
	assert!(!copied.contains("nested"), "{copied}");
	// Its relative path, too.
	right_click("commit-file:alpha.txt");
	wait("[APP:MENU_OPEN: Log items=copy-files,");
	click("menu-item:copy-relative-path");
	wait("[APP:TEXT_COPIED:");
	assert_eq!(clip_get(), "alpha.txt");
	let type_text = |text: &str| {
		Command::new("xdotool")
			.args(["type", "--window", &wid, text])
			.status()
			.unwrap();
	};
	// 5c. Branch chip: Local is a closed section; typing lists the
	// matching branches flat.
	click("log-filter-branch");
	wait("[APP:LOG_MENU: Some(Branch)]");
	control("log-branch-group:refs_local");
	absent("log-branch:refs/heads/main");
	click("log-branch-group:refs_local");
	control("log-branch:refs/heads/main");
	click("log-branch-group:refs_local");
	absent("log-branch:refs/heads/main");
	type_text("MAI");
	control("log-branch:refs/heads/main");
	absent("log-branch-group:refs_local");
	key(&wid, "Escape");
	absent("log-branch-input");
	// 5d. Paths chip: typing filters the loaded tree.
	click("log-filter-paths");
	wait("[APP:LOG_MENU: Some(Paths)]");
	control("log-path-pick:alpha.txt");
	type_text("nest");
	control("log-path-pick:subfolder/nested.txt");
	absent("log-path-pick:alpha.txt");
	key(&wid, "Escape");
	absent("log-path-input");

	// 6. Ctrl+Shift+` opens the branches popup.
	key(&wid, "ctrl+shift+grave");
	wait("[APP:SELECTOR_OPEN: Ref");
	control("pick-ref:refs/heads/main");
	key(&wid, "Escape");
	wait("[APP:SELECTOR_CLOSED]");

	// 7. Changes groups are tree nodes: the group checkbox selects the
	// whole group, the chevron collapses it. Untracked files list under
	// Unstaged and keep their own source in the row id.
	key(&wid, "alt+0");
	wait("[APP:TAB_SWITCHED: GitChanges visible=true");
	click("change-group-chk:unstaged");
	let toggled = wait("[APP:GROUP_TOGGLED: unstaged selected=true]");
	let basket = toggled
		.iter()
		.find(|l| l.contains("[APP:BASKET: n=3"))
		.unwrap_or_else(|| {
			panic!("group toggle must fill the basket: {toggled:?}")
		});
	assert!(
		basket.contains("gamma.txt") && basket.contains("delta.txt"),
		"{basket}"
	);
	control("change-row:untracked:gamma.txt");
	click("change-group-toggle:unstaged");
	wait("[APP:GROUP_COLLAPSED: unstaged collapsed=true]");
	absent("change-row:untracked:gamma.txt");
	control("change-header:unstaged");

	quit_cleanly(&mut app, &wid);
}

/// Changes of a multi-repo workspace are grouped by change kind first: a
/// workspace-wide Staged / Unstaged node (untracked files list under
/// Unstaged) over one row per repo with files in it, and under each repo
/// row its files grouped by directory, like IntelliJ's "Group By >
/// Directory". A clean repo is not listed; repo rows and directories start
/// collapsed and expand per group; a chain of single-child directories is
/// one row; a directory's checkbox covers every file beneath it; the
/// header toggle switches to flat lists and back. A file of a repo that is
/// not the open one previews from its own repo and checks into that repo's
/// basket entry, and the group checkbox spans every repo.
#[test]
fn native_changes_group_all_repos() {
	changes_group_all_repos("dark");
}

/// The same tree in the light palette, for the screenshot only.
#[test]
fn native_changes_group_all_repos_light() {
	changes_group_all_repos("light");
}

fn changes_group_all_repos(theme: &str) {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let xdotool = Command::new("xdotool").arg("--version").output();
	if xdotool.is_err() || !xdotool.unwrap().status.success() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let init = |name: &str, nested: &[&str]| {
		let repo = ws.path().join(name);
		fs::create_dir_all(&repo).unwrap();
		git_ok(&repo, &["init", "-q", "-b", "main"]);
		git_ok(&repo, &["config", "user.email", "t@example.com"]);
		git_ok(&repo, &["config", "user.name", "Multi Repo"]);
		fs::write(repo.join("shared.txt"), format!("{name}\n")).unwrap();
		for path in nested {
			let file = repo.join(path);
			fs::create_dir_all(file.parent().unwrap()).unwrap();
			fs::write(file, "v1\n").unwrap();
		}
		git_ok(&repo, &["add", "."]);
		git_ok(&repo, &["commit", "-qm", "first"]);
		repo
	};
	// `src` has two children; `main/java/pkg` is a single-child chain.
	let nested = ["src/main/java/pkg/App.java", "src/test/AppTest.java"];
	let alpha = init("alpha", &nested);
	let beta = init("beta", &[]);
	init("gamma", &[]);
	// alpha: one staged file, and unstaged edits at the root and nested.
	fs::write(alpha.join("staged.txt"), "staged\n").unwrap();
	git_ok(&alpha, &["add", "staged.txt"]);
	fs::write(alpha.join("shared.txt"), "alpha edited\n").unwrap();
	for path in nested {
		fs::write(alpha.join(path), "v2\n").unwrap();
	}
	// beta: a staged and an unstaged change and an untracked folder of two
	// files, on a feature branch.
	git_ok(&beta, &["checkout", "-qb", "feature/x"]);
	fs::write(beta.join("beta-staged.txt"), "beta staged\n").unwrap();
	git_ok(&beta, &["add", "beta-staged.txt"]);
	fs::write(beta.join("shared.txt"), "beta edited\n").unwrap();
	fs::create_dir_all(beta.join("newdir")).unwrap();
	fs::write(beta.join("newdir/one.txt"), "one\ntwo\nthree\n").unwrap();
	fs::write(beta.join("newdir/two.txt"), "two\n").unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app_themed(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
		theme,
	);
	let rx = &app.rx;
	let loaded = lines_until_all_smoke(
		rx,
		&[
			"[APP:REPO_LOADED: alpha files=4]",
			"[APP:CHANGES_LOADED: beta files=4]",
			"[APP:CHANGES_LOADED: gamma files=0]",
		],
		Duration::from_secs(15),
	);
	assert!(
		!loaded.iter().any(|l| l.contains("CHANGES_LOADED: alpha")),
		"the open repo's rows are its own load's: {loaded:?}"
	);
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	let st = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status()
		.unwrap();
	assert!(st.success());
	let wait = |pattern: &str| {
		lines_until(rx, pattern, Duration::from_secs(6))
			.unwrap_or_else(|e| panic!("{pattern}: {e}"))
	};
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if let Some(v) = settled().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0
						&& v[3] > 0 && v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} was not drawn");
		}
	};
	let absent = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(4);
		while bounds.lock().unwrap().contains_key(id) {
			assert!(Instant::now() < deadline, "{id} must not be drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				"1",
			])
			.status()
			.unwrap();
		assert!(st.success());
	};

	// Groups first, each over the repos with files in it, in name order;
	// the clean repo, empty (group, repo) pairs and an Untracked group are
	// not listed. Repo rows start collapsed.
	let order = [
		"change-header:staged",
		"change-repo:staged:alpha",
		"change-repo:staged:beta",
		"change-header:unstaged",
		"change-repo:unstaged:alpha",
		"change-repo:unstaged:beta",
	];
	let ys: Vec<i32> = order.iter().map(|id| control(id)[1]).collect();
	assert!(ys.windows(2).all(|w| w[0] < w[1]), "{order:?} {ys:?}");
	let snap = settled();
	assert!(
		!snap
			.keys()
			.any(|k| k.starts_with("change-") && k.contains("untracked")),
		"{snap:?}"
	);
	assert!(
		!snap
			.keys()
			.any(|k| k.starts_with("change-") && k.contains("gamma")),
		"{snap:?}"
	);
	absent("change-row@alpha:staged:staged.txt");
	absent("change-row@beta:unstaged:shared.txt");

	// Repo rows expand per group.
	for (group, name) in [
		("staged", "alpha"),
		("staged", "beta"),
		("unstaged", "alpha"),
		("unstaged", "beta"),
	] {
		click(&format!("change-repo:{group}:{name}"));
		wait(&format!(
			"[APP:REPO_CHANGES_COLLAPSED: {group} {name} collapsed=false]"
		));
	}
	control("change-row@alpha:staged:staged.txt");
	control("change-row@beta:staged:beta-staged.txt");
	control("change-row@beta:unstaged:shared.txt");
	// Under an open repo: its top-level directories, collapsed, then its
	// root files. The untracked folder sits in Unstaged.
	let src = "change-dir:unstaged:alpha:src";
	let newdir = "change-dir:unstaged:beta:newdir";
	assert!(
		control(src)[1] < control("change-row@alpha:unstaged:shared.txt")[1]
	);
	assert!(
		control(newdir)[1] < control("change-row@beta:unstaged:shared.txt")[1]
	);
	absent("change-dir:unstaged:alpha:src/test");
	absent("change-row@beta:untracked:newdir/one.txt");
	// Unqualified ids keep naming the open repo's rows only.
	assert_eq!(
		control("change-row:staged:staged.txt"),
		control("change-row@alpha:staged:staged.txt")
	);
	// The file rows sit under their repo row, indented past its checkbox.
	let repo_chk = control("change-repo-chk:staged:beta");
	let file_chk = control("change-chk@beta:staged:beta-staged.txt");
	assert!(file_chk[0] > repo_chk[0], "{file_chk:?} {repo_chk:?}");

	// One click opens a directory; the log carries its file count. `src`
	// shows the compacted chain and `test`, both collapsed.
	click(src);
	wait("[APP:CHANGE_DIR_COLLAPSED: unstaged alpha src files=2 collapsed=false]");
	let chain = "change-dir:unstaged:alpha:src/main/java/pkg";
	let test_dir = "change-dir:unstaged:alpha:src/test";
	control(test_dir);
	control(chain);
	absent("change-dir:unstaged:alpha:src/main");
	absent("change-row@alpha:unstaged:src/main/java/pkg/App.java");
	click(chain);
	wait(
		"[APP:CHANGE_DIR_COLLAPSED: unstaged alpha src/main/java/pkg files=1 collapsed=false]",
	);
	let app_row = "change-row@alpha:unstaged:src/main/java/pkg/App.java";
	control(app_row);
	absent("change-row@alpha:unstaged:src/test/AppTest.java");
	click(newdir);
	wait("[APP:CHANGE_DIR_COLLAPSED: unstaged beta newdir files=2 collapsed=false]");
	control("change-row@beta:untracked:newdir/one.txt");
	control("change-row@beta:untracked:newdir/two.txt");
	// Chevrons of one level share a column, a child's sits right of its
	// parent's, and sibling files line up with sibling directories'
	// checkboxes.
	let x = |id: &str| control(id)[0];
	let level2 = x("change-dir-toggle:unstaged:alpha:src");
	let level3 = x("change-dir-toggle:unstaged:alpha:src/main/java/pkg");
	assert_eq!(level3, x("change-dir-toggle:unstaged:alpha:src/test"));
	assert_eq!(level2, x("change-dir-toggle:unstaged:beta:newdir"));
	assert!(level3 > level2, "{level3} {level2}");
	assert_eq!(
		x("change-dir-chk:unstaged:alpha:src"),
		x("change-chk@alpha:unstaged:shared.txt")
	);
	assert_eq!(
		x("change-dir-chk:unstaged:alpha:src/test"),
		x("change-chk@alpha:unstaged:src/main/java/pkg/App.java")
			- (level3 - level2)
	);

	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let shot = if theme == "dark" {
		"multi-repo-changes.png".to_string()
	} else {
		format!("multi-repo-changes-{theme}.png")
	};
	// Hide the Git log so the whole tree fits, and park the pointer over
	// the editor so no row tooltip covers the tree.
	key(&wid, "alt+9");
	wait("[APP:LOG_PANEL: visible=false]");
	let st = Command::new("xdotool")
		.args(["mousemove", "--window", &wid, "700", "300"])
		.status()
		.unwrap();
	assert!(st.success());
	std::thread::sleep(Duration::from_millis(800));
	capture_window(&wid, &out.join(shot));
	if theme != "dark" {
		quit_cleanly(&mut app, &wid);
		return;
	}

	// A file of the repo that is not open previews from its own repo (the
	// new-file diff of its three lines; alpha has no such file to read).
	click("change-row@beta:untracked:newdir/one.txt");
	let shown = wait(
		"[APP:E2E_PREVIEW: source=working_changes rev=- path=newdir/one.txt ",
	);
	assert!(
		shown.last().unwrap().contains(" lines=6 "),
		"beta's file must be read from beta: {shown:?}"
	);
	wait("[APP:PREVIEW_LOADED: newdir/one.txt]");

	// Its checkbox fills that repo's basket entry, still as untracked.
	click("change-chk@beta:untracked:newdir/one.txt");
	let basket = wait("[APP:BASKET: n=1");
	assert!(
		basket
			.last()
			.unwrap()
			.contains("beta untracked newdir/one.txt"),
		"{basket:?}"
	);
	// A repo row's checkbox covers that repo's files of that group only.
	click("change-repo-chk:staged:beta");
	let basket = wait("[APP:BASKET: n=2");
	wait("[APP:REPO_CHANGES_TOGGLED: staged beta selected=true]");
	assert!(!basket.last().unwrap().contains("alpha"), "{basket:?}");
	// A directory's checkbox covers every file beneath it, collapsed
	// `test` included, and nothing beside it.
	click("change-dir-chk:unstaged:alpha:src");
	let basket = wait("[APP:BASKET: n=4");
	wait("[APP:DIR_CHANGES_TOGGLED: unstaged alpha src selected=true]");
	let last = basket.last().unwrap();
	assert!(
		last.contains("alpha unstaged src/main/java/pkg/App.java")
			&& last.contains("alpha unstaged src/test/AppTest.java")
			&& !last.contains("alpha unstaged shared.txt"),
		"{basket:?}"
	);
	// A group's checkbox spans every repo, untracked files included.
	click("change-group-chk:unstaged");
	let basket = wait("[APP:BASKET: n=7");
	wait("[APP:GROUP_TOGGLED: unstaged selected=true]");
	let last = basket.last().unwrap();
	assert!(
		last.contains("alpha unstaged shared.txt")
			&& last.contains("beta unstaged shared.txt")
			&& last.contains("beta untracked newdir/two.txt"),
		"{basket:?}"
	);

	// The header toggle lists files flat under each repo, and back.
	click("btn-changes-group-dir");
	wait("[APP:CHANGES_GROUP_DIR: on=false]");
	absent(src);
	control("change-row@alpha:unstaged:src/test/AppTest.java");
	control(app_row);
	click("btn-changes-group-dir");
	wait("[APP:CHANGES_GROUP_DIR: on=true]");
	control(src);
	// Directory expansion survives the round trip.
	control(app_row);
	absent("change-row@alpha:unstaged:src/test/AppTest.java");

	// Collapsing a repo row hides its files in that group only.
	click("change-repo:unstaged:beta");
	wait("[APP:REPO_CHANGES_COLLAPSED: unstaged beta collapsed=true]");
	absent(newdir);
	absent("change-row@beta:untracked:newdir/one.txt");
	control("change-row@beta:staged:beta-staged.txt");
	// One click on a group row collapses it; the chevron reopens it.
	click("change-header:staged");
	wait("[APP:GROUP_COLLAPSED: staged collapsed=true]");
	absent("change-repo:staged:alpha");
	absent("change-row@beta:staged:beta-staged.txt");
	control("change-repo:unstaged:alpha");
	click("change-group-toggle:staged");
	wait("[APP:GROUP_COLLAPSED: staged collapsed=false]");
	control("change-row@beta:staged:beta-staged.txt");

	quit_cleanly(&mut app, &wid);
}

/// Every pattern must appear (in any order) before `timeout`.
fn lines_until_all_smoke(
	rx: &Receiver<String>,
	patterns: &[&str],
	timeout: Duration,
) -> Vec<String> {
	let deadline = Instant::now() + timeout;
	let mut seen: Vec<String> = Vec::new();
	while !patterns.iter().all(|p| seen.iter().any(|l| l.contains(p))) {
		let left = deadline.saturating_duration_since(Instant::now());
		assert!(!left.is_zero(), "missing {patterns:?}; saw {seen:?}");
		match rx.recv_timeout(left.min(Duration::from_millis(50))) {
			Ok(line) => seen.push(line),
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => {
				panic!("app exited; saw {seen:?}")
			}
		}
	}
	seen
}

/// A workspace of several repositories has one log, like IntelliJ with
/// several VCS roots: every repository's commits interleaved by date, a
/// root stripe per row, a Repository chip that narrows the log, and the
/// details and diff of a commit read from its own repository.
#[test]
fn native_multi_repo_log_merges_and_filters_repositories() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	// Commit dates interleave the repositories; the offsets differ, so an
	// order by date string would be wrong.
	let plan: [(&str, [&str; 3]); 3] = [
		(
			"alpha",
			[
				"2026-01-01T01:00:00+00:00",
				"2026-01-04T01:00:00+00:00",
				"2026-01-07T01:00:00+00:00",
			],
		),
		(
			"beta",
			[
				"2026-01-02T09:00:00+08:00",
				"2026-01-05T09:00:00+08:00",
				"2026-01-08T09:00:00+08:00",
			],
		),
		(
			"gamma",
			[
				"2026-01-02T20:00:00-05:00",
				"2026-01-05T20:00:00-05:00",
				"2026-01-08T20:00:00-05:00",
			],
		),
	];
	let mut shas: HashMap<String, String> = HashMap::new();
	for (name, dates) in plan {
		let repo = ws.path().join(name);
		fs::create_dir_all(&repo).unwrap();
		git_ok(&repo, &["init", "-q", "-b", "main"]);
		git_ok(&repo, &["config", "user.email", "t@example.com"]);
		git_ok(&repo, &["config", "user.name", "Multi Test"]);
		for (n, date) in dates.iter().enumerate() {
			let n = n + 1;
			fs::write(repo.join(format!("{name}{n}.txt")), format!("{n}\n"))
				.unwrap();
			git_ok(&repo, &["add", "."]);
			let out = Command::new("git")
				.current_dir(&repo)
				.args(["commit", "-qm", &format!("{name} change {n}")])
				.env("GIT_AUTHOR_DATE", date)
				.env("GIT_COMMITTER_DATE", date)
				.output()
				.unwrap();
			assert!(out.status.success());
			shas.insert(format!("{name}{n}"), git_rev(&repo));
		}
	}
	git_ok(&ws.path().join("beta"), &["branch", "feature"]);
	let row = |c: &str| {
		let name = c.trim_end_matches(char::is_numeric);
		format!("commit-row:{name}:{}", &shas[c][..7])
	};
	// Newest first across the three repositories.
	let order = [
		"gamma3", "beta3", "alpha3", "gamma2", "beta2", "alpha2", "gamma1",
		"beta1", "alpha1",
	];

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	lines_until(&app.rx, "[APP:READY_REPOS: 3]", Duration::from_secs(12))
		.expect("repos must load");
	lines_until(
		&app.rx,
		"[APP:MULTI_LOG_LOADED: repos=3 rows=9]",
		Duration::from_secs(12),
	)
	.expect("the merged log must load every repository");
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	let st = Command::new("xdotool")
		.args(["windowmove", "--sync", &wid, "0", "0"])
		.status()
		.unwrap();
	assert!(st.success());
	let st = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1280", "860"])
		.status()
		.unwrap();
	assert!(st.success());
	let rx = &app.rx;
	let wait = |pattern: &str| {
		lines_until(rx, pattern, Duration::from_secs(8))
			.unwrap_or_else(|e| panic!("{pattern}: {e}"))
	};
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if let Some(v) = settled().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0
						&& v[3] > 0 && v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} was not drawn");
		}
	};
	let absent = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(6);
		while bounds.lock().unwrap().contains_key(id) {
			assert!(Instant::now() < deadline, "{id} must not be drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let press = |id: &str, modifier: Option<&str>| {
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		if let Some(m) = modifier {
			Command::new("xdotool")
				.args(["keydown", m])
				.status()
				.unwrap();
		}
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				"1",
			])
			.status()
			.unwrap();
		if let Some(m) = modifier {
			Command::new("xdotool").args(["keyup", m]).status().unwrap();
		}
		assert!(st.success());
	};
	let click = |id: &str| press(id, None);

	// A taller log: every row on screen (and in the picture).
	{
		let v = control("splitter-bottom");
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let xdo = |args: &[&str]| {
			assert!(Command::new("xdotool")
				.args(args)
				.status()
				.unwrap()
				.success())
		};
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
		]);
		xdo(&["mousedown", "1"]);
		for step in 1..=4 {
			let y = (y - 60 * step).to_string();
			xdo(&["mousemove", "--window", &wid, &x.to_string(), &y]);
			std::thread::sleep(Duration::from_millis(30));
		}
		xdo(&["mouseup", "1"]);
		wait("[APP:SPLIT_RESIZED: Bottom");
	}
	// 1. One log, newest first, each row with its repository's stripe.
	let ys: Vec<i32> = order.iter().map(|c| control(&row(c))[1]).collect();
	assert!(
		ys.windows(2).all(|w| w[0] < w[1]),
		"rows must be ordered newest first across repositories: {:?}",
		order.iter().zip(&ys).collect::<Vec<_>>()
	);
	for c in order {
		let stripe = control(&format!("root-stripe:{}", &row(c)[11..]));
		let r = control(&row(c));
		assert_eq!((stripe[0], stripe[1]), (r[0], r[1]), "{c} stripe");
		assert!(stripe[2] < 12, "the stripe is a thin column: {stripe:?}");
	}
	// The selected repository's rows keep their plain-SHA ids for drivers.
	control(&format!("commit-row:{}", &shas["alpha3"][..7]));
	absent(&format!("commit-row:{}", &shas["beta3"][..7]));
	control("log-filter-repo");

	// 2. A commit's details and changes come from its own repository.
	click(&row("beta2"));
	wait(&format!("[APP:COMMIT_SELECTED: {}]", &shas["beta2"][..7]));
	control("commit-details-repo:beta");
	control("commit-file:beta2.txt");
	// A range across repositories is refused, not merged.
	press(&row("gamma2"), Some("shift"));
	wait("[APP:RANGE_REFUSED: cross_repo]");

	// 3. The merged log as a picture (dark theme).
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let shot = out.join("multi-repo-log.png");
	{
		let geometry = Command::new("xdotool")
			.args(["getwindowgeometry", "--shell", &wid])
			.output()
			.unwrap();
		let text = String::from_utf8(geometry.stdout).unwrap();
		let g: HashMap<_, _> =
			text.lines().filter_map(|l| l.split_once('=')).collect();
		let crop =
			format!("{}x{}+{}+{}", g["WIDTH"], g["HEIGHT"], g["X"], g["Y"]);
		settled();
		std::thread::sleep(Duration::from_millis(300));
		let xwd = tempfile::Builder::new().suffix(".xwd").tempfile().unwrap();
		assert!(Command::new("xwd")
			.args(["-root", "-silent", "-out"])
			.arg(xwd.path())
			.status()
			.unwrap()
			.success());
		assert!(Command::new("convert")
			.arg(xwd.path())
			.args(["-crop", &crop, "+repage"])
			.arg(&shot)
			.status()
			.unwrap()
			.success());
		assert!(fs::metadata(&shot).unwrap().len() > 1024);
	}
	println!("[TEST DRIVER] merged log screenshot: {}", shot.display());

	// 4. The Repository chip narrows the log to one repository: then it is
	// the single-repository log with plain row ids.
	click("log-filter-repo");
	click("log-repo-check:alpha");
	wait("[APP:LOG_REPOS: n=2]");
	wait("[APP:MULTI_LOG_LOADED: repos=2 rows=6]");
	absent(&row("alpha3"));
	// A row click keeps only its repository.
	click("log-repo:beta");
	wait("[APP:LOG_REPOS: n=1]");
	let loaded = wait("[APP:E2E_LOG:");
	assert!(
		loaded
			.last()
			.unwrap()
			.contains(&format!("mode=graph n=3 first={}", &shas["beta3"][..7])),
		"{loaded:?}"
	);
	control(&format!("commit-row:{}", &shas["beta1"][..7]));
	absent(&row("gamma3"));
	absent(&format!("root-stripe:{}", &row("beta3")[11..]));
	// Back to every repository.
	click("log-filter-repo-clear");
	wait("[APP:MULTI_LOG_LOADED: repos=3 rows=9]");
	control(&row("alpha1"));

	quit_cleanly(&mut app, &wid);
}

/// A long CJK name is cut with "…" and then measured again at another width
/// (a window or panel resize). gpui used to truncate the shared text runs,
/// so the next measure laid out the full text with the cut runs: on macOS
/// that sliced inside a multi-byte char and aborted the app. The vendored
/// gpui asserts in debug builds that runs cover the text, which fails here
/// on Linux too.
#[test]
fn native_cjk_truncation_survives_resize() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();
	let ws = tempfile::tempdir().unwrap();
	let repo = ws.path().join("repo");
	fs::create_dir(&repo).unwrap();
	git_ok(&repo, &["init", "-q", "-b", "main"]);
	git_ok(&repo, &["config", "user.name", "CJK Test"]);
	git_ok(&repo, &["config", "user.email", "cjk@example.com"]);
	git_ok(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
	fs::create_dir(repo.join("報表資料夾")).unwrap();
	fs::write(
		repo.join("報表資料夾/第一季財務報告與分析結果彙整文件.md"),
		"y\n",
	)
	.unwrap();
	fs::write(
		repo.join("這是一個非常非常長的中文檔案名稱用來測試截斷與省略號.txt"),
		"x\n",
	)
	.unwrap();
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app(ws.path(), dest.path(), None);
	lines_until(&app.rx, "[APP:PREVIEW_LOADED:", Duration::from_secs(10))
		.unwrap();
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	for w in ["1200", "700", "1300", "600", "1440", "800", "500", "1100"] {
		assert!(Command::new("xdotool")
			.args(["windowsize", &wid, w, "800"])
			.status()
			.unwrap()
			.success());
		std::thread::sleep(Duration::from_millis(400));
		let child = app.child.as_mut().expect("app still owned");
		assert!(
			child.try_wait().unwrap().is_none(),
			"app died while laying out CJK text at width {w}"
		);
	}
	quit_cleanly(&mut app, &wid);
}

/// IntelliJ's log selection: Cmd/Ctrl-click toggles commits in and out of a
/// selection that may have gaps, the details pane lists the union of their
/// changed files as the Changes tool window's tree, Copy Commits refuses a
/// selection that is not one first-parent chain, and another repository's
/// commit cannot join.
#[test]
fn native_log_multiselect() {
	log_multiselect("dark");
}

/// The same selection in the light palette, for the screenshot only.
#[test]
fn native_log_multiselect_light() {
	log_multiselect("light");
}

fn log_multiselect(theme: &str) {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	// alpha: four commits, beta: two, interleaved by date.
	// Per repository: (commit date, files it writes) oldest first.
	type Commits = &'static [(&'static str, &'static [&'static str])];
	let plan: [(&str, Commits); 2] = [
		(
			"alpha",
			&[
				(
					"2026-01-01T01:00:00+00:00",
					&["README.md", "src/app/main.rs"],
				),
				(
					"2026-01-03T01:00:00+00:00",
					&["src/app/main.rs", "docs/guide.md"],
				),
				("2026-01-05T01:00:00+00:00", &["lib/util.rs"]),
				(
					"2026-01-07T01:00:00+00:00",
					&["src/app/main.rs", "src/app/view/list.rs"],
				),
			],
		),
		(
			"beta",
			&[
				("2026-01-02T01:00:00+00:00", &["b.txt"]),
				("2026-01-06T01:00:00+00:00", &["b.txt"]),
			],
		),
	];
	let mut shas: HashMap<String, String> = HashMap::new();
	for (name, commits) in plan {
		let repo = ws.path().join(name);
		fs::create_dir_all(&repo).unwrap();
		git_ok(&repo, &["init", "-q", "-b", "main"]);
		git_ok(&repo, &["config", "user.email", "t@example.com"]);
		git_ok(&repo, &["config", "user.name", "Multi Select"]);
		for (n, (date, files)) in commits.iter().enumerate() {
			let n = n + 1;
			for f in *files {
				let p = repo.join(f);
				fs::create_dir_all(p.parent().unwrap()).unwrap();
				fs::write(&p, format!("{name} {n}\n")).unwrap();
			}
			git_ok(&repo, &["add", "."]);
			let out = Command::new("git")
				.current_dir(&repo)
				.args(["commit", "-qm", &format!("{name} change {n}")])
				.env("GIT_AUTHOR_DATE", date)
				.env("GIT_COMMITTER_DATE", date)
				.output()
				.unwrap();
			assert!(out.status.success());
			shas.insert(format!("{name}{n}"), git_rev(&repo));
		}
	}
	let row = |c: &str| {
		let name = c.trim_end_matches(char::is_numeric);
		format!("commit-row:{name}:{}", &shas[c][..7])
	};

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app_themed(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
		theme,
	);
	lines_until(&app.rx, "[APP:READY_REPOS: 2]", Duration::from_secs(12))
		.expect("repos must load");
	lines_until(
		&app.rx,
		"[APP:MULTI_LOG_LOADED: repos=2 rows=6]",
		Duration::from_secs(12),
	)
	.expect("the merged log must load every repository");
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	for args in [
		vec!["windowmove", "--sync", &wid, "0", "0"],
		vec!["windowsize", "--sync", &wid, "1280", "860"],
	] {
		assert!(Command::new("xdotool")
			.args(args)
			.status()
			.unwrap()
			.success());
	}
	let rx = &app.rx;
	let wait = |pattern: &str| {
		lines_until(rx, pattern, Duration::from_secs(8))
			.unwrap_or_else(|e| panic!("{pattern}: {e}"))
	};
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if let Some(v) = settled().get(id).copied() {
				let (vw, vh) = *viewport.lock().unwrap();
				assert!(
					v[2] > 0
						&& v[3] > 0 && v[0] >= 0
						&& v[1] >= 0 && v[0] + v[2] <= vw
						&& v[1] + v[3] <= vh,
					"{id} {v:?} outside {vw}x{vh}"
				);
				return v;
			}
			assert!(Instant::now() < deadline, "{id} was not drawn");
		}
	};
	let absent = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(6);
		while bounds.lock().unwrap().contains_key(id) {
			assert!(Instant::now() < deadline, "{id} must not be drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let press = |id: &str, modifier: Option<&str>| {
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		if let Some(m) = modifier {
			Command::new("xdotool")
				.args(["keydown", m])
				.status()
				.unwrap();
		}
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				"1",
			])
			.status()
			.unwrap();
		if let Some(m) = modifier {
			Command::new("xdotool").args(["keyup", m]).status().unwrap();
		}
		assert!(st.success());
	};
	let click = |id: &str| press(id, None);
	// Drawn, maybe scrolled past the details pane's edge.
	let drawn = |id: &str| {
		let deadline = Instant::now() + Duration::from_secs(6);
		while !bounds.lock().unwrap().contains_key(id) {
			assert!(Instant::now() < deadline, "{id} was not drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let (a2, a4) = (&shas["alpha2"][..7], &shas["alpha4"][..7]);

	// A taller log: every row and the whole details tree on screen.
	{
		let v = control("splitter-bottom");
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let xdo = |args: &[&str]| {
			assert!(Command::new("xdotool")
				.args(args)
				.status()
				.unwrap()
				.success())
		};
		xdo(&[
			"mousemove",
			"--window",
			&wid,
			&x.to_string(),
			&y.to_string(),
		]);
		xdo(&["mousedown", "1"]);
		for step in 1..=4 {
			let y = (y - 60 * step).to_string();
			xdo(&["mousemove", "--window", &wid, &x.to_string(), &y]);
			std::thread::sleep(Duration::from_millis(30));
		}
		xdo(&["mouseup", "1"]);
		wait("[APP:SPLIT_RESIZED: Bottom");
	}

	// 1. One commit, then Ctrl-click a second one with a gap between.
	click(&row("alpha4"));
	wait("[APP:LOG_SELECTION: n=1 repo=alpha]");
	press(&row("alpha2"), Some("ctrl"));
	wait("[APP:LOG_SELECTION: n=2 repo=alpha]");
	// The union of both commits' files, newest change per path; alpha3's
	// lib/util.rs sits in the gap and is not listed.
	wait("[APP:E2E_CHANGES: files=3]");
	for f in ["src/app/main.rs", "src/app/view/list.rs", "docs/guide.md"] {
		control(&format!("commit-file:{f}"));
	}
	absent("commit-file:lib/util.rs");
	absent("commit-file:README.md");
	// The Changes tree: `src/app` is one row, directories start open.
	control("commit-dir:src/app");
	control("commit-dir:docs");
	control("commit-details-selection");
	// The first file opens as the selection's diff.
	wait("[APP:PREVIEW_LOADED:");
	// The changed files get most of the right pane, the details less.
	let (files, details) =
		(control("commit-files-pane"), control("commit-details"));
	assert!(files[3] > details[3], "files {files:?} details {details:?}");
	// The selected commits stay one header row until it is clicked open;
	// each then shows its author line and branches, not just a subject.
	control("log-selection-toggle");
	absent(&format!("selection-commit:{a4}"));
	click("log-selection-toggle");
	wait("[APP:LOG_SELECTION_EXPANDED: true]");
	wait("[APP:SELECTION_DETAILS: 2]");
	for sha in [a4, a2] {
		drawn(&format!("selection-commit:{sha}"));
		drawn(&format!("commit-details-author:{sha}"));
		drawn(&format!("commit-details-branches:{sha}"));
	}
	click("log-selection-toggle");
	wait("[APP:LOG_SELECTION_EXPANDED: false]");
	absent(&format!("selection-commit:{a4}"));
	// Flat and back.
	click("details-group-dir");
	wait("[APP:LOG_DETAILS_GROUP_DIR: false]");
	absent("commit-dir:src/app");
	control("commit-file:docs/guide.md");
	click("details-group-dir");
	wait("[APP:LOG_DETAILS_GROUP_DIR: true]");
	control("commit-dir:src/app");
	// A file opens over the selected range.
	click("commit-file:docs/guide.md");
	wait("[APP:PREVIEW_LOADED: docs/guide.md]");

	// 2. Copy Commits keeps its rule: one first-parent chain.
	click("btn-copy-commits");
	wait("[APP:COPY_COMMITS_ERR: commits are not contiguous:");

	// 3. Another repository's commit cannot join the selection.
	press(&row("beta2"), Some("ctrl"));
	wait("[APP:RANGE_REFUSED: cross_repo]");
	control("commit-details-selection");

	// 4. The picture: two selected rows, the union as a tree.
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let shot = out.join(if theme == "light" {
		"log-multiselect-light.png"
	} else {
		"log-multiselect.png"
	});
	settled();
	std::thread::sleep(Duration::from_millis(300));
	capture_window(&wid, &shot);
	println!(
		"[TEST DRIVER] log multi-select screenshot: {}",
		shot.display()
	);

	// 5. Ctrl-click toggles out: back to one commit.
	press(&row("alpha2"), Some("ctrl"));
	wait(&format!("[APP:COMMIT_SELECTED: {}]", &shas["alpha4"][..7]));
	absent("commit-details-selection");
	// One commit: hash, author and email on one line, then its branches.
	wait(&format!("[APP:COMMIT_DETAILS: {a4} branches=1]"));
	drawn(&format!("commit-details-author:{a4}"));
	drawn(&format!("commit-details-branches:{a4}"));

	quit_cleanly(&mut app, &wid);
}

/// The merged log's graph over four repositories with branches, merges,
/// root commits and a clone sharing SHAs, interleaved by date. Every
/// commit's row sits above its parents' rows in its own repository (the
/// order `git log --graph --topo-order` draws), and the picture is kept
/// as `merged-graph.png` for review.
#[test]
fn native_merged_graph_over_four_repositories() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	let _gui = gui_lock();

	let ws = tempfile::tempdir().unwrap();
	let at = |day: u32| format!("2026-02-{day:02}T10:00:00+00:00");
	let run = |repo: &Path, day: u32, args: &[&str]| {
		let out = Command::new("git")
			.current_dir(repo)
			.args(args)
			.env("GIT_AUTHOR_DATE", at(day))
			.env("GIT_COMMITTER_DATE", at(day))
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	};
	let commit = |repo: &Path, day: u32, msg: &str| {
		fs::write(repo.join(format!("{msg}.txt")), format!("{msg}\n")).unwrap();
		run(repo, day, &["add", "."]);
		run(repo, day, &["commit", "-qm", msg]);
	};
	let init = |name: &str| {
		let repo = ws.path().join(name);
		fs::create_dir_all(&repo).unwrap();
		git_ok(&repo, &["init", "-q", "-b", "main"]);
		git_ok(&repo, &["config", "user.email", "g@example.com"]);
		git_ok(&repo, &["config", "user.name", "Graph"]);
		repo
	};
	// alpha: a feature branch merged back with --no-ff.
	let alpha = init("alpha");
	commit(&alpha, 1, "a1");
	commit(&alpha, 3, "a2");
	git_ok(&alpha, &["checkout", "-q", "-b", "feat"]);
	commit(&alpha, 8, "af2");
	git_ok(&alpha, &["checkout", "-q", "main"]);
	commit(&alpha, 6, "a3");
	run(&alpha, 9, &["merge", "-q", "--no-ff", "-m", "am", "feat"]);
	commit(&alpha, 11, "a4");
	// delta: a clone of alpha's first two commits (same SHAs), then its own.
	let delta = ws.path().join("delta");
	let st = Command::new("git")
		.args(["clone", "-q", "--no-local"])
		.arg(&alpha)
		.arg(&delta)
		.status()
		.unwrap();
	assert!(st.success());
	git_ok(&delta, &["config", "user.email", "g@example.com"]);
	git_ok(&delta, &["config", "user.name", "Graph"]);
	git_ok(&delta, &["reset", "-q", "--hard", "HEAD~3"]);
	git_ok(&delta, &["remote", "remove", "origin"]);
	commit(&delta, 10, "d2");
	// beta: the newest commit of all, and a root in the middle: its lane
	// ends while every other repository goes on below it.
	let beta = init("beta");
	commit(&beta, 2, "b1");
	commit(&beta, 12, "b3");
	// gamma: an unmerged topic branch and an orphan second root.
	let gamma = init("gamma");
	commit(&gamma, 2, "c1");
	git_ok(&gamma, &["checkout", "-q", "-b", "topic"]);
	commit(&gamma, 8, "ct1");
	git_ok(&gamma, &["checkout", "-q", "main"]);
	git_ok(&gamma, &["checkout", "-q", "--orphan", "pages"]);
	git_ok(&gamma, &["rm", "-rfq", "."]);
	commit(&gamma, 5, "cp1");
	git_ok(&gamma, &["checkout", "-q", "main"]);
	commit(&gamma, 10, "c3");

	let rows = |repo: &Path| -> Vec<(String, Vec<String>)> {
		let out = Command::new("git")
			.current_dir(repo)
			.args(["rev-list", "--parents", "--all"])
			.output()
			.unwrap();
		String::from_utf8(out.stdout)
			.unwrap()
			.lines()
			.map(|l| {
				let mut it = l.split(' ').map(str::to_string);
				(it.next().unwrap(), it.collect())
			})
			.collect()
	};
	let repos = [
		("alpha", alpha.clone()),
		("beta", beta.clone()),
		("delta", delta.clone()),
		("gamma", gamma.clone()),
	];
	let total: usize = repos.iter().map(|(_, r)| rows(r).len()).sum();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let dest = tempfile::tempdir().unwrap();
	let mut app = spawn_app(
		ws.path(),
		dest.path(),
		Some((bounds.clone(), viewport.clone())),
	);
	lines_until(&app.rx, "[APP:READY_REPOS: 4]", Duration::from_secs(12))
		.expect("repos must load");
	lines_until(
		&app.rx,
		&format!("[APP:MULTI_LOG_LOADED: repos=4 rows={total}]"),
		Duration::from_secs(12),
	)
	.expect("the merged log must load every repository");
	let wid = find_wid(app.pid);
	key(&wid, "Escape");
	for args in [
		vec!["windowmove", "--sync", &wid, "0", "0"],
		vec!["windowsize", "--sync", &wid, "1280", "900"],
	] {
		assert!(Command::new("xdotool")
			.args(args)
			.status()
			.unwrap()
			.success());
	}
	let rx = &app.rx;
	let settled = || {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			let snap = bounds.lock().unwrap().clone();
			std::thread::sleep(Duration::from_millis(150));
			if *bounds.lock().unwrap() == snap {
				return snap;
			}
			assert!(Instant::now() < deadline, "layout never settled");
		}
	};
	// Every row on screen: the log takes most of the window. The resize
	// above may not be laid out yet on a slow runner, so a drag that missed
	// the splitter re-reads its bounds and tries again.
	{
		let xdo = |args: &[&str]| {
			assert!(Command::new("xdotool")
				.args(args)
				.status()
				.unwrap()
				.success())
		};
		let mut resized = Err(String::new());
		for _ in 0..3 {
			let v = *settled().get("splitter-bottom").expect("splitter");
			let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
			xdo(&[
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
			]);
			std::thread::sleep(Duration::from_millis(100));
			xdo(&["mousedown", "1"]);
			for step in 1..=6 {
				let y = (y - 60 * step).to_string();
				xdo(&["mousemove", "--window", &wid, &x.to_string(), &y]);
				std::thread::sleep(Duration::from_millis(30));
			}
			xdo(&["mouseup", "1"]);
			resized = lines_until(
				rx,
				"[APP:SPLIT_RESIZED: Bottom",
				Duration::from_secs(5),
			);
			if resized.is_ok() {
				break;
			}
		}
		resized.unwrap();
	}
	let out = std::env::var_os("SNIP_E2E_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("target/native-e2e-artifacts"));
	fs::create_dir_all(&out).unwrap();
	let shot = out.join("merged-graph.png");
	std::thread::sleep(Duration::from_millis(300));
	capture_window(&wid, &shot);
	println!("[TEST DRIVER] merged graph screenshot: {}", shot.display());

	// Each repository's commits sit above their parents.
	let snap = settled();
	for (name, repo) in &repos {
		let y = |sha: &str| {
			let id = format!("commit-row:{name}:{}", &sha[..7]);
			snap.get(&id).unwrap_or_else(|| panic!("{id} not drawn"))[1]
		};
		for (sha, parents) in rows(repo) {
			for p in parents {
				assert!(y(&sha) < y(&p), "{name}: {sha} must sit above {p}");
			}
		}
	}

	quit_cleanly(&mut app, &wid);
}

/// The Project view shows the whole workspace like IntelliJ: a plain
/// folder beside a repo is listed, its file previews, checks into the
/// basket and copies as a file-mode payload. The repo keeps its row.
#[test]
fn native_project_view_lists_and_copies_non_git_files() {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		return;
	}
	if Command::new("xdotool").arg("--version").output().is_err() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but xdotool is not available"
		);
		return;
	}
	let _gui = gui_lock();

	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let app_repo = ws.join("app");
	fs::create_dir_all(&app_repo).unwrap();
	git_ok(&app_repo, &["init", "-q", "-b", "main"]);
	git_ok(&app_repo, &["config", "user.name", "Tester"]);
	git_ok(&app_repo, &["config", "user.email", "test@example.com"]);
	fs::write(app_repo.join("main.rs"), "fn main() {}\n").unwrap();
	git_ok(&app_repo, &["add", "."]);
	git_ok(&app_repo, &["commit", "-qm", "base"]);
	fs::create_dir_all(ws.join("notes")).unwrap();
	fs::write(ws.join("notes/readme.txt"), "PLAIN_NOTE_BYTES\n").unwrap();
	fs::write(ws.join("top.txt"), "TOP_BYTES\n").unwrap();
	let dest = tmp.path().join("dest");
	fs::create_dir_all(&dest).unwrap();

	let bounds: Bounds = Arc::new(Mutex::new(HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let mut app =
		spawn_app(&ws, &dest, Some((bounds.clone(), viewport.clone())));
	let rx = &app.rx;
	let wait_for = |pattern: &str, timeout: Duration| -> String {
		lines_until(rx, pattern, timeout)
			.unwrap_or_else(|e| panic!("{e}"))
			.pop()
			.unwrap()
	};
	wait_for("[APP:READY_REPOS: 1]", Duration::from_secs(8));
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	let control = |id: &str| -> [i32; 4] {
		let deadline = Instant::now() + Duration::from_secs(6);
		loop {
			if let Some(v) = bounds.lock().unwrap().get(id).copied() {
				if v[2] > 0 && v[3] > 0 {
					return v;
				}
			}
			assert!(Instant::now() < deadline, "{id} not drawn");
			std::thread::sleep(Duration::from_millis(40));
		}
	};
	let click = |id: &str| {
		std::thread::sleep(Duration::from_millis(200));
		let v = control(id);
		let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
		let _ = Command::new("xdotool")
			.args(["windowfocus", "--sync", &wid])
			.status();
		let st = Command::new("xdotool")
			.args([
				"mousemove",
				"--window",
				&wid,
				&x.to_string(),
				&y.to_string(),
				"click",
				"1",
			])
			.status()
			.unwrap();
		assert!(st.success(), "click {id}");
	};

	click("rail-project");
	wait_for("[APP:TAB_SWITCHED: FileExplorer", Duration::from_secs(3));
	// The repo keeps its row (and probe id) inside the workspace listing.
	control("repo-row:app");
	control("ws-tree-row:top.txt");
	click("ws-tree-row:notes");
	wait_for("[APP:WS_TREE_PAGE: rel=notes", Duration::from_secs(4));
	click("ws-tree-row:notes/readme.txt");
	wait_for(
		"[APP:PREVIEW_LOADED: notes/readme.txt]",
		Duration::from_secs(4),
	);
	click("ws-tree-chk:notes/readme.txt");
	wait_for("[APP:BASKET: n=1", Duration::from_secs(3));
	key(&wid, "ctrl+c");
	wait_for("[APP:COPY_DONE: copied=1]", Duration::from_secs(6));
	let copied = clip::read_text().unwrap();
	assert!(copied.contains("PLAIN_NOTE_BYTES"), "{copied}");
	assert!(copied.contains("notes/readme.txt"), "{copied}");
	assert!(!copied.contains("TOP_BYTES"), "{copied}");

	// A repo file and a workspace file copy together as one payload.
	click("tree-chk:main.rs");
	wait_for("[APP:BASKET: n=2", Duration::from_secs(3));
	key(&wid, "ctrl+c");
	wait_for("[APP:COPY_DONE: copied=2]", Duration::from_secs(6));
	let both = clip::read_text().unwrap();
	assert!(both.contains("PLAIN_NOTE_BYTES"), "{both}");
	assert!(both.contains("fn main() {}"), "{both}");
	for line in both
		.lines()
		.filter(|l| l.contains("readme.txt") || l.contains("main.rs"))
	{
		println!("[MIXED HEADER] {line}");
	}
	quit_cleanly(&mut app, &wid);
}
