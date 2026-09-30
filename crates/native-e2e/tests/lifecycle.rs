#![cfg(target_os = "linux")]
//! Real X11 checks for workspace close/reopen and quit drain.
//!
//! Run under `scripts/headless-x11.sh` so this never touches the user's
//! DISPLAY. Clicks use bounds the app reports. A timeout kills the process
//! only as cleanup and fails the test; that is not a graceful exit.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snip_native_e2e::{native_bin, scaled};

type Bounds = Arc<Mutex<std::collections::HashMap<String, [i32; 4]>>>;
type Viewport = Arc<Mutex<(i32, i32)>>;

/// Serializes the tests that drive the shared display, across threads and
/// processes. An `flock` on an open file, not a lock directory: the kernel
/// drops it when the holder exits, so a killed run cannot block later ones.
struct DisplayLock(#[allow(dead_code)] fs::File);

impl DisplayLock {
	fn acquire() -> Self {
		let path =
			std::env::temp_dir().join("snip-native-lifecycle-display.flock");
		let file = fs::OpenOptions::new()
			.create(true)
			.truncate(false)
			.write(true)
			.open(&path)
			.expect("lifecycle display lock file");
		let start = Instant::now();
		loop {
			match file.try_lock() {
				Ok(()) => return Self(file),
				Err(fs::TryLockError::WouldBlock) => {}
				Err(fs::TryLockError::Error(e)) => {
					panic!("lifecycle display lock {path:?}: {e}")
				}
			}
			if start.elapsed() > scaled(Duration::from_secs(120)) {
				panic!("timed out waiting for the lifecycle display lock");
			}
			std::thread::sleep(Duration::from_millis(200));
		}
	}
}

struct App {
	child: Option<Child>,
	rx: Receiver<String>,
	pid: u32,
	starttime: String,
	tracked: Vec<(u32, String)>,
	readers: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for App {
	fn drop(&mut self) {
		let _ = Command::new("xdotool")
			.args(["keyup", "ctrl", "alt", "shift", "super", "q"])
			.status();
		if let Some(mut child) = self.child.take() {
			eprintln!(
				"[LIFECYCLE-TEST] cleanup kill of pid {} (failure path, not a graceful exit)",
				self.pid
			);
			let _ = child.kill();
			let _ = child.wait();
		}
		for (pid, start) in &self.tracked {
			if same_proc(*pid, start) {
				eprintln!(
					"[LIFECYCLE-TEST] cleanup kill of leftover child {pid} (not a graceful exit)"
				);
				let _ = Command::new("kill")
					.args(["-KILL", &pid.to_string()])
					.status();
			}
		}
		join_readers(self);
	}
}

fn join_readers(app: &mut App) {
	for handle in app.readers.drain(..) {
		let _ = handle.join();
	}
}

fn require_display_tools() -> bool {
	if std::env::var_os("DISPLAY").is_none() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but no X11 DISPLAY is available"
		);
		eprintln!("skip: no DISPLAY");
		return false;
	}
	for tool in ["xdotool", "xclip", "xwd", "convert", "git", "cc"] {
		let found = Command::new("which")
			.arg(tool)
			.output()
			.is_ok_and(|out| out.status.success());
		assert!(found, "required tool {tool} is missing");
	}
	true
}

fn real_git() -> PathBuf {
	let out = Command::new("which")
		.arg("git")
		.output()
		.expect("which git");
	assert!(out.status.success(), "git is not on PATH");
	PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

fn git(bin: &Path, dir: &Path, args: &[&str]) {
	let out = Command::new(bin)
		.args(args)
		.current_dir(dir)
		.output()
		.expect("git");
	assert!(
		out.status.success(),
		"git {args:?} in {}: {}",
		dir.display(),
		String::from_utf8_lossy(&out.stderr)
	);
}

fn init_repo(bin: &Path, dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(bin, dir, &["init", "-q"]);
	git(bin, dir, &["config", "user.email", "lifecycle@example.com"]);
	git(bin, dir, &["config", "user.name", "Lifecycle"]);
	fs::write(dir.join("README"), "init\n").unwrap();
	git(bin, dir, &["add", "README"]);
	git(bin, dir, &["commit", "-q", "-m", "init"]);
}

fn proc_rest(pid: u32) -> Option<Vec<String>> {
	let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
	let after = stat.rfind(')')?;
	Some(
		stat[after + 2..]
			.split_whitespace()
			.map(|s| s.to_string())
			.collect(),
	)
}

fn starttime(pid: u32) -> Option<String> {
	proc_rest(pid)?.get(19).cloned()
}

fn same_proc(pid: u32, start: &str) -> bool {
	starttime(pid).as_deref() == Some(start)
}

fn descendants(root: u32) -> Vec<u32> {
	let mut edges = Vec::new();
	let Ok(rd) = fs::read_dir("/proc") else {
		return Vec::new();
	};
	for ent in rd.flatten() {
		let Ok(pid) = ent.file_name().to_string_lossy().parse::<u32>() else {
			continue;
		};
		let Some(rest) = proc_rest(pid) else {
			continue;
		};
		let Some(ppid) = rest.get(1).and_then(|s| s.parse::<u32>().ok()) else {
			continue;
		};
		edges.push((pid, ppid));
	}
	let mut out = Vec::new();
	let mut stack = vec![root];
	while let Some(parent) = stack.pop() {
		for (pid, ppid) in &edges {
			if *ppid == parent {
				out.push(*pid);
				stack.push(*pid);
			}
		}
	}
	out
}

fn cmdline(pid: u32) -> String {
	fs::read(format!("/proc/{pid}/cmdline"))
		.map(|bytes| {
			String::from_utf8_lossy(&bytes)
				.replace('\0', " ")
				.trim()
				.to_string()
		})
		.unwrap_or_default()
}

fn snapshot(pid: u32, start: &str) {
	let kids: Vec<String> = descendants(pid)
		.into_iter()
		.map(|child| format!("{child}:{}", cmdline(child)))
		.collect();
	println!(
		"[LIFECYCLE-TEST] pid={pid} starttime={start} alive={} children={kids:?}",
		same_proc(pid, start)
	);
}

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

struct SpawnOpts<'a> {
	workspace: &'a Path,
	restore: &'a Path,
	read_delay_ms: Option<u64>,
	apply_delay_ms: Option<u64>,
	hold_file: Option<&'a Path>,
	path_prefix: Option<&'a Path>,
	tree_hold: Option<&'a Path>,
	export_hold: Option<&'a Path>,
}

fn spawn_app(opts: SpawnOpts) -> App {
	let mut cmd = Command::new(native_bin());
	cmd.args([
		"--workspace",
		&opts.workspace.to_string_lossy(),
		"--restore-dir",
		&opts.restore.to_string_lossy(),
	])
	.stdout(Stdio::piped())
	.stderr(Stdio::piped())
	.env("XMODIFIERS", "@im=none")
	.env("SNIP_THEME", "dark")
	.env("SNIP_NATIVE_E2E", "1")
	.env_remove("SNIP_E2E_GIT_HOLD_FILE")
	.env_remove("SNIP_NATIVE_E2E_READ_DELAY_MS")
	.env_remove("SNIP_NATIVE_E2E_APPLY_DELAY_MS")
	.env_remove("SNIP_NATIVE_E2E_TREE_HOLD_FILE")
	.env_remove("SNIP_NATIVE_E2E_EXPORT_HOLD_FILE");
	if let Some(ms) = opts.read_delay_ms {
		cmd.env("SNIP_NATIVE_E2E_READ_DELAY_MS", ms.to_string());
	}
	if let Some(ms) = opts.apply_delay_ms {
		cmd.env("SNIP_NATIVE_E2E_APPLY_DELAY_MS", ms.to_string());
	}
	if let Some(hold) = opts.hold_file {
		cmd.env("SNIP_E2E_GIT_HOLD_FILE", hold);
	}
	if let Some(hold) = opts.tree_hold {
		cmd.env("SNIP_NATIVE_E2E_TREE_HOLD_FILE", hold);
	}
	if let Some(hold) = opts.export_hold {
		cmd.env("SNIP_NATIVE_E2E_EXPORT_HOLD_FILE", hold);
	}
	if let Some(prefix) = opts.path_prefix {
		let old = std::env::var_os("PATH").unwrap_or_default();
		let mut path = std::ffi::OsString::from(prefix);
		path.push(":");
		path.push(old);
		cmd.env("PATH", path);
	}
	let bounds: Bounds = Arc::new(Mutex::new(std::collections::HashMap::new()));
	let viewport: Viewport = Arc::new(Mutex::new((0, 0)));
	let probes = (Arc::clone(&bounds), Arc::clone(&viewport));
	let mut child = cmd.spawn().expect("native desktop should run");
	let pid = child.id();
	let starttime = starttime(pid).expect("app starttime");
	let stdout = child.stdout.take().unwrap();
	let (tx, rx) = std::sync::mpsc::channel::<String>();
	let stdout_reader = std::thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if let Some((id, v)) = parse_bounds(&line) {
				probes.0.lock().unwrap().insert(id, v);
				continue;
			}
			if let Some(id) = parse_gone(&line) {
				probes.0.lock().unwrap().remove(&id);
				continue;
			}
			if let Some(vp) = parse_viewport(&line) {
				*probes.1.lock().unwrap() = vp;
			}
			println!("[APP STDOUT] {line}");
			let _ = tx.send(line);
		}
	});
	let stderr = child.stderr.take().unwrap();
	let stderr_reader = std::thread::spawn(move || {
		for line in BufReader::new(stderr).lines().map_while(Result::ok) {
			eprintln!("[APP STDERR] {line}");
		}
	});
	// Stash maps on the App via a side channel? The click helpers need them.
	// Keep them process-local by leaking into thread-locals set before return.
	BOUNDS.with(|slot| *slot.borrow_mut() = Some(bounds));
	VIEW.with(|slot| *slot.borrow_mut() = Some(viewport));
	snapshot(pid, &starttime);
	App {
		child: Some(child),
		rx,
		pid,
		starttime,
		tracked: Vec::new(),
		readers: vec![stdout_reader, stderr_reader],
	}
}

thread_local! {
	static BOUNDS: std::cell::RefCell<Option<Bounds>> = const { std::cell::RefCell::new(None) };
	static VIEW: std::cell::RefCell<Option<Viewport>> = const { std::cell::RefCell::new(None) };
}

fn lines_until(
	rx: &Receiver<String>,
	pattern: &str,
	timeout: Duration,
) -> Vec<String> {
	let deadline = Instant::now() + scaled(timeout);
	let mut seen = Vec::new();
	while Instant::now() < deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		match rx.recv_timeout(remaining.min(Duration::from_millis(40))) {
			Ok(line) => {
				let hit = line.contains(pattern);
				seen.push(line);
				if hit {
					return seen;
				}
			}
			Err(RecvTimeoutError::Timeout) => continue,
			Err(RecvTimeoutError::Disconnected) => break,
		}
	}
	panic!("timed out after {timeout:?} waiting for '{pattern}'; saw {seen:?}");
}

fn find_wid(pid: u32) -> String {
	let start = Instant::now();
	while start.elapsed() < scaled(Duration::from_secs(8)) {
		let out = Command::new("xdotool")
			.args(["search", "--pid", &pid.to_string()])
			.output()
			.expect("xdotool");
		if let Some(w) = String::from_utf8_lossy(&out.stdout)
			.lines()
			.find(|l| !l.trim().is_empty())
		{
			return w.trim().to_string();
		}
		// The child is not reaped yet, so an app that died stays a zombie.
		if proc_rest(pid).is_none_or(|rest| rest[0] == "Z") {
			panic!(
				"app pid {pid} exited before mapping a window (DISPLAY={:?}); see its [APP STDERR] above",
				std::env::var_os("DISPLAY")
			);
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	panic!("xdotool did not find a window for pid {pid}");
}

fn focus(wid: &str) {
	let _ = Command::new("xdotool")
		.args(["windowmap", "--sync", wid])
		.status();
	let st = Command::new("xdotool")
		.args(["windowfocus", "--sync", wid])
		.status()
		.expect("xdotool focus");
	assert!(st.success(), "windowfocus failed");
	std::thread::sleep(Duration::from_millis(40));
}

fn key(wid: &str, keys: &str) {
	focus(wid);
	let st = Command::new("xdotool")
		.args(["key", "--window", wid, keys])
		.status()
		.expect("xdotool key");
	assert!(st.success(), "key {keys} failed");
}

fn control(id: &str) -> [i32; 4] {
	let deadline = Instant::now() + scaled(Duration::from_secs(4));
	loop {
		let found = BOUNDS.with(|slot| {
			slot.borrow().as_ref().and_then(|b| {
				snip_native_e2e::lookup_bounds(&b.lock().unwrap(), id)
			})
		});
		if let Some(v) = found {
			let (vw, vh) = VIEW.with(|slot| {
				slot.borrow()
					.as_ref()
					.map(|v| *v.lock().unwrap())
					.unwrap_or((0, 0))
			});
			assert!(v[2] > 0 && v[3] > 0, "{id} empty {v:?}");
			assert!(
				vw > 0
					&& vh > 0 && v[0] >= 0
					&& v[1] >= 0 && v[0] + v[2] <= vw
					&& v[1] + v[3] <= vh,
				"{id} bounds {v:?} outside {vw}x{vh}"
			);
			return v;
		}
		assert!(Instant::now() < deadline, "control {id} was not drawn");
		std::thread::sleep(Duration::from_millis(40));
	}
}

fn click(wid: &str, id: &str) {
	std::thread::sleep(Duration::from_millis(180));
	focus(wid);
	let v = control(id);
	let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
	println!("[LIFECYCLE-TEST] click {id} at ({x},{y}) bounds {v:?}");
	let st = Command::new("xdotool")
		.args([
			"mousemove",
			"--window",
			wid,
			&x.to_string(),
			&y.to_string(),
			"click",
			"1",
		])
		.status()
		.expect("xdotool click");
	assert!(st.success(), "click {id} failed");
}

/// Ctrl-click: the Project view's multi-selection toggle.
fn ctrl_click(wid: &str, id: &str) {
	std::thread::sleep(Duration::from_millis(180));
	focus(wid);
	let v = control(id);
	let (x, y) = (v[0] + v[2] / 2, v[1] + v[3] / 2);
	let st = Command::new("xdotool")
		.args([
			"mousemove",
			"--window",
			wid,
			&x.to_string(),
			&y.to_string(),
			"keydown",
			"ctrl",
			"click",
			"1",
			"keyup",
			"ctrl",
		])
		.status()
		.expect("xdotool ctrl-click");
	assert!(st.success(), "ctrl-click {id} failed");
}

fn capture(wid: &str, path: &Path) {
	if let Some(parent) = path.parent() {
		let _ = fs::create_dir_all(parent);
	}
	let xwd = path.with_extension("xwd");
	let mut len = 0;
	for _ in 0..8 {
		focus(wid);
		let st = Command::new("xwd")
			.args(["-id", wid, "-silent", "-out"])
			.arg(&xwd)
			.status()
			.expect("xwd");
		assert!(st.success(), "xwd failed for {}", path.display());
		// `convert` exists in ImageMagick 6 (CI) and 7; `magick` is 7 only.
		let st = Command::new("convert")
			.arg(&xwd)
			.args(["-depth", "8", &format!("PNG24:{}", path.display())])
			.status()
			.expect("convert");
		assert!(st.success(), "convert failed for {}", path.display());
		len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
		if len > 15_000 {
			break;
		}
		std::thread::sleep(Duration::from_millis(250));
	}
	let _ = fs::remove_file(&xwd);
	assert!(
		len > 15_000,
		"screenshot {} is blank or tiny ({len} bytes)",
		path.display()
	);
	println!(
		"[LIFECYCLE-TEST] screenshot {} ({} bytes)",
		path.display(),
		len
	);
}

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

fn wait_exit(app: &mut App, timeout: Duration) -> std::process::ExitStatus {
	let mut child = app.child.take().expect("app still owned");
	let start = Instant::now();
	let status = loop {
		match child.try_wait().expect("try_wait") {
			Some(status) => break status,
			None if start.elapsed() < scaled(timeout) => {
				std::thread::sleep(Duration::from_millis(30));
			}
			None => {
				let _ = child.kill();
				let _ = child.wait();
				join_readers(app);
				panic!(
					"app pid {} did not exit within {timeout:?}; killed as cleanup, not a graceful exit",
					app.pid
				);
			}
		}
	};
	join_readers(app);
	status
}

/// Screenshots go next to the smoke artifacts when the runner names a
/// directory, so CI uploads them; otherwise to a fixed local directory.
fn shot_dir(local: &str) -> PathBuf {
	match std::env::var_os("SNIP_E2E_OUT") {
		Some(out) if !out.is_empty() => PathBuf::from(out).join("lifecycle"),
		_ => PathBuf::from(local),
	}
}

fn shots() -> PathBuf {
	shot_dir("/tmp/snip-native-lifecycle-20260926-shots")
}

#[test]
fn close_reopen_same_pid_discards_stale_preview_and_keeps_clipboard() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws_a = root.path().join("ws-a");
	let ws_b = root.path().join("ws-b");
	let repo_a = ws_a.join("repo-a");
	let repo_b = ws_b.join("repo-b");
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	init_repo(&git_bin, &repo_a);
	init_repo(&git_bin, &repo_b);
	fs::write(repo_a.join("note.txt"), "apply-bytes\n").unwrap();
	fs::write(repo_b.join("other.txt"), "from-b\n").unwrap();

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws_a,
		restore: &dest,
		read_delay_ms: Some(2500),
		apply_delay_ms: Some(2000),
		hold_file: None,
		path_prefix: None,
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(12));
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	let _ = control("btn-workspace-menu");
	// Jobs show in the status bar only while some run (no "0" counter).
	let _ = control("status-bar");
	capture(&wid, &shots().join("01-workspace-open.png"));
	snapshot(app.pid, &app.starttime);

	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:FILE_TOGGLED:", Duration::from_secs(4));
	click(&wid, "btn-copy");
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(8));
	click(&wid, "btn-paste");
	lines_until(&app.rx, "[APP:PASTE_PREVIEW:", Duration::from_secs(6));
	click(&wid, "btn-apply");
	lines_until(&app.rx, "[APP:PASTE_APPLYING]", Duration::from_secs(4));
	key(&wid, "ctrl+q");
	let refused = lines_until(
		&app.rx,
		"phase=refused intent=quit reason=applying",
		Duration::from_secs(4),
	);
	assert!(
		refused.iter().any(|l| l.contains("[APP:QUIT: deferred]")),
		"quit must be deferred, not cx.quit: {refused:?}"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"quit during apply exited"
	);
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	lines_until(
		&app.rx,
		"phase=refused intent=close-workspace reason=applying",
		Duration::from_secs(4),
	);
	capture(&wid, &shots().join("02-apply-busy.png"));
	lines_until(&app.rx, "[APP:PASTE_DONE:", Duration::from_secs(8));
	let written =
		fs::read(dest.join("note.txt")).expect("apply wrote note.txt");
	assert_eq!(
		written, b"apply-bytes",
		"wire format drops the trailing newline"
	);
	assert!(same_proc(app.pid, &app.starttime));

	let sentinel = "CLIP-SENTINEL-lifecycle-20260926";
	clip_set(sentinel);
	assert_eq!(clip_get(), sentinel);
	click(&wid, "change-row:note.txt");
	lines_until(
		&app.rx,
		"[APP:PREVIEW_LOADING: note.txt]",
		Duration::from_secs(4),
	);
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	std::thread::sleep(Duration::from_millis(200));
	capture(&wid, &shots().join("03-draining.png"));
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(8),
	);
	let discarded = closed.iter().position(|l| {
		l.contains("[APP:PREVIEW_DISCARDED: stale path=note.txt]")
	});
	let closed_at = closed
		.iter()
		.position(|l| l.contains("[APP:WORKSPACE: state=closed"));
	assert!(
		discarded.is_some() && closed_at.is_some() && discarded < closed_at,
		"close finished before the in-flight preview settled: {closed:?}"
	);
	assert!(
		closed.iter().any(|l| {
			l.contains("phase=drained intent=close-workspace")
				&& l.contains("jobs=0")
				&& l.contains("inflight=0")
				&& l.contains("queued=0")
				&& l.contains("leaked=0")
		}),
		"close did not report a real drain: {closed:?}"
	);
	assert_eq!(
		clip_get(),
		sentinel,
		"clipboard changed across workspace close"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"close exited the process"
	);
	let _ = control("workspace-closed");
	std::thread::sleep(Duration::from_millis(700));
	capture(&wid, &shots().join("04-workspace-closed.png"));
	snapshot(app.pid, &app.starttime);

	click(&wid, "btn-open-workspace");
	click(&wid, "workspace-path-input");
	focus(&wid);
	let st = Command::new("xdotool")
		.args([
			"type",
			"--delay",
			"15",
			"--window",
			&wid,
			&ws_b.to_string_lossy(),
		])
		.status()
		.expect("xdotool type");
	assert!(st.success(), "typing the workspace path failed");
	click(&wid, "btn-workspace-open-confirm");
	let opened = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=open path=",
		Duration::from_secs(6),
	);
	assert!(
		opened
			.iter()
			.any(|l| l.contains(&ws_b.to_string_lossy().to_string())),
		"opened path was not workspace B: {opened:?}"
	);
	lines_until(&app.rx, "[APP:READY_REPOS:", Duration::from_secs(12));
	let mut after = Vec::new();
	let deadline = Instant::now() + scaled(Duration::from_millis(800));
	while Instant::now() < deadline {
		match app.rx.recv_timeout(Duration::from_millis(50)) {
			Ok(line) => after.push(line),
			Err(_) => continue,
		}
	}
	assert!(
		after
			.iter()
			.all(|l| !l.contains("[APP:PREVIEW_LOADED: note.txt]")),
		"stale note.txt preview appeared in the new workspace: {after:?}"
	);
	assert_eq!(clip_get(), sentinel);
	assert!(
		same_proc(app.pid, &app.starttime),
		"reopen changed the process"
	);
	capture(&wid, &shots().join("05-workspace-reopened.png"));
	snapshot(app.pid, &app.starttime);

	request_wm_delete(&wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
	assert!(
		!same_proc(app.pid, &app.starttime),
		"process still alive after quit"
	);
	assert!(
		descendants(app.pid).is_empty(),
		"owned children survived quit"
	);
}

#[test]
fn quit_drains_a_held_git_child_before_the_process_exits() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("repo");
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	init_repo(&git_bin, &repo);
	let wrap_dir = root.path().join("bin");
	fs::create_dir_all(&wrap_dir).unwrap();
	let script = format!(
		"#!/bin/sh\nreal={}\nhold=\"${{SNIP_E2E_GIT_HOLD_FILE:-}}\"\nhold_this=0\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"status\" ]; then hold_this=1; fi\ndone\nif [ \"$hold_this\" = 1 ] && [ -n \"$hold\" ] && [ -f \"$hold\" ]; then\n  while [ -f \"$hold\" ]; do\n    sleep 0.05\n  done\nfi\nexec \"$real\" \"$@\"\n",
		git_bin.display()
	);
	let wrap = wrap_dir.join("git");
	fs::write(&wrap, script).unwrap();
	let mut perm = fs::metadata(&wrap).unwrap().permissions();
	perm.set_mode(0o755);
	fs::set_permissions(&wrap, perm).unwrap();
	let hold = root.path().join("hold");

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: Some(&hold),
		path_prefix: Some(&wrap_dir),
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until(&app.rx, "[APP:READY_REPOS:", Duration::from_secs(12));
	fs::write(&hold, b"hold").unwrap();
	key(&wid, "ctrl+r");
	let git_child = wait_git_child(app.pid, &app.starttime);
	app.tracked.push(git_child.clone());
	println!(
		"[LIFECYCLE-TEST] in-flight git pid={} starttime={} cmd={}",
		git_child.0,
		git_child.1,
		cmdline(git_child.0)
	);
	assert!(same_proc(app.pid, &app.starttime));
	assert!(same_proc(git_child.0, &git_child.1));
	key(&wid, "ctrl+q");
	let status = {
		let start = Instant::now();
		let mut child = app.child.take().unwrap();
		loop {
			let app_alive = same_proc(app.pid, &app.starttime);
			let git_alive = same_proc(git_child.0, &git_child.1);
			if !app_alive && git_alive {
				let _ = child.kill();
				let _ = child.wait();
				panic!(
					"process exited while the owned git child was still alive (immediate cx.quit)"
				);
			}
			match child.try_wait().unwrap() {
				Some(st) => break st,
				None if start.elapsed() < scaled(Duration::from_secs(10)) => {
					std::thread::sleep(Duration::from_millis(15));
				}
				None => {
					let _ = child.kill();
					let _ = child.wait();
					panic!(
						"quit did not finish within 10s (cleanup kill, not graceful)"
					);
				}
			}
		}
	};
	assert!(status.success(), "quit status {status}");
	assert!(
		!same_proc(git_child.0, &git_child.1),
		"git child survived quit; hold file was not removed, so only cancellation could stop it"
	);
	assert!(!same_proc(app.pid, &app.starttime));
	let mut lines = Vec::new();
	while let Ok(line) = app.rx.try_recv() {
		lines.push(line);
	}
	assert!(
		lines.iter().any(|l| l.contains("[APP:QUIT: deferred]")),
		"missing deferred quit: {lines:?}"
	);
	assert!(
		lines
			.iter()
			.any(|l| l.contains("phase=draining intent=quit")),
		"missing drain: {lines:?}"
	);
	assert!(
		lines.iter().any(|l| l.contains("phase=drained intent=quit")
			&& l.contains("inflight=0")
			&& l.contains("jobs=0")),
		"missing drained evidence: {lines:?}"
	);
	assert!(
		lines.iter().all(|l| !l.contains("[APP:QUIT: immediate]")),
		"immediate quit marker appeared: {lines:?}"
	);
	snapshot(app.pid, &app.starttime);
}

fn integration_shots() -> PathBuf {
	shot_dir("/tmp/snip-native-lifecycle-export-integration-20260926-shots")
}

fn hold_all_git_wrapper(git_bin: &Path, wrap: &Path) {
	let script = format!(
		"#!/bin/sh\nreal={}\nhold=\"${{SNIP_E2E_GIT_HOLD_FILE:-}}\"\nif [ -n \"$hold\" ] && [ -f \"$hold\" ]; then\n  while [ -f \"$hold\" ]; do\n    sleep 0.05\n  done\nfi\nexec \"$real\" \"$@\"\n",
		git_bin.display()
	);
	if let Some(parent) = wrap.parent() {
		fs::create_dir_all(parent).unwrap();
	}
	fs::write(wrap, script).unwrap();
	let mut perm = fs::metadata(wrap).unwrap().permissions();
	perm.set_mode(0o755);
	fs::set_permissions(wrap, perm).unwrap();
}

fn git_busy(pid: u32) -> bool {
	let cmd = cmdline(pid);
	cmd.contains("git") || cmd.contains("sleep")
}

fn wait_git_idle(app_pid: u32, app_start: &str) {
	let deadline = Instant::now() + scaled(Duration::from_secs(8));
	let mut since: Option<Instant> = None;
	while Instant::now() < deadline {
		assert!(
			same_proc(app_pid, app_start),
			"app exited while waiting for Git to go idle"
		);
		let busy = descendants(app_pid).into_iter().any(git_busy);
		if busy {
			since = None;
		} else if let Some(start) = since {
			if start.elapsed() >= Duration::from_millis(300) {
				return;
			}
		} else {
			since = Some(Instant::now());
		}
		std::thread::sleep(Duration::from_millis(30));
	}
	let kids: Vec<_> = descendants(app_pid)
		.into_iter()
		.map(|pid| format!("{pid}:{}", cmdline(pid)))
		.collect();
	panic!("git did not go idle before the copy; children={kids:?}");
}

fn wait_copy_git(app_pid: u32, app_start: &str) -> (u32, String) {
	let deadline = Instant::now() + scaled(Duration::from_secs(6));
	while Instant::now() < deadline {
		assert!(
			same_proc(app_pid, app_start),
			"app died before copy's git child appeared"
		);
		for pid in descendants(app_pid) {
			let cmd = cmdline(pid);
			let copy_git = cmd.contains("git")
				&& (cmd.contains("--version")
					|| cmd.contains("rev-parse")
					|| cmd.contains("show-toplevel"));
			if copy_git {
				if let Some(start) = starttime(pid) {
					return (pid, start);
				}
			}
		}
		std::thread::sleep(Duration::from_millis(20));
	}
	let kids: Vec<_> = descendants(app_pid)
		.into_iter()
		.map(|pid| format!("{pid}:{}", cmdline(pid)))
		.collect();
	panic!("copy did not start a git child; children={kids:?}");
}

/// In-flight copy: Git has already been spawned, then close cancels it.
#[test]
fn close_cancels_in_flight_copy_without_writing_clipboard() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws_a = root.path().join("ws-a");
	let ws_b = root.path().join("ws-b");
	let repo_a = ws_a.join("repo-a");
	let repo_b = ws_b.join("repo-b");
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	init_repo(&git_bin, &repo_a);
	init_repo(&git_bin, &repo_b);
	fs::write(repo_a.join("note.txt"), "copy-bytes\n").unwrap();
	fs::write(repo_b.join("other.txt"), "from-b\n").unwrap();
	let wrap = root.path().join("bin").join("git");
	hold_all_git_wrapper(&git_bin, &wrap);
	let hold = root.path().join("hold");

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws_a,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: Some(&hold),
		path_prefix: Some(wrap.parent().unwrap()),
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(12));
	lines_until(&app.rx, "[APP:REPO_LOADED:", Duration::from_secs(8));
	lines_until(&app.rx, "[APP:PREVIEW_LOADED:", Duration::from_secs(8));
	wait_git_idle(app.pid, &app.starttime);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	let sentinel = "CLIP-SENTINEL-copy-cancel-20260926";
	clip_set(sentinel);
	assert_eq!(clip_get(), sentinel);
	fs::write(&hold, b"hold").unwrap();
	assert!(
		hold.is_file(),
		"hold file must exist before copy so the git child blocks inside the call"
	);
	assert!(
		descendants(app.pid).into_iter().all(|pid| !git_busy(pid)),
		"a git child existed before copy; that would not prove the copy token"
	);

	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:FILE_TOGGLED:", Duration::from_secs(4));
	click(&wid, "btn-copy");
	let prep = lines_until(&app.rx, "[APP:COPY_PREP:", Duration::from_secs(4));
	assert!(
		prep.iter().all(|l| !l.contains("[APP:COPY_DONE:")),
		"copy finished before its git child was observed: {prep:?}"
	);
	let git_child = wait_copy_git(app.pid, &app.starttime);
	app.tracked.push(git_child.clone());
	let git_cmd = cmdline(git_child.0);
	println!(
		"[LIFECYCLE-TEST] in-flight copy git pid={} starttime={} cmd={}",
		git_child.0, git_child.1, git_cmd
	);
	assert!(
		same_proc(git_child.0, &git_child.1),
		"copy git exited before cancel"
	);
	assert!(same_proc(app.pid, &app.starttime));
	capture(&wid, &integration_shots().join("01-copy-inflight.png"));

	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(12),
	);
	assert!(
		closed
			.iter()
			.any(|l| l.contains("[APP:COPY_DISCARDED: cancelled]")),
		"close did not discard the in-flight copy: {closed:?}"
	);
	assert!(
		closed.iter().all(|l| !l.contains("[APP:COPY_DONE:")),
		"cancelled copy still wrote the clipboard marker: {closed:?}"
	);
	assert!(
		closed.iter().any(|l| {
			l.contains("phase=drained intent=close-workspace")
				&& l.contains("jobs=0")
				&& l.contains("inflight=0")
				&& l.contains("queued=0")
				&& l.contains("leaked=0")
		}),
		"close did not report a real drain: {closed:?}"
	);
	assert!(
		hold.is_file(),
		"hold file was removed; the child could have finished instead of being cancelled"
	);
	assert!(
		!same_proc(git_child.0, &git_child.1),
		"git child survived cancel; cmd was {git_cmd}"
	);
	let leftover: Vec<_> = descendants(app.pid)
		.into_iter()
		.filter(|pid| git_busy(*pid))
		.map(|pid| format!("{pid}:{}", cmdline(pid)))
		.collect();
	assert!(
		leftover.is_empty(),
		"git descendants remained after cancel: {leftover:?}"
	);
	assert_eq!(clip_get(), sentinel, "clipboard changed after cancel");
	assert!(
		same_proc(app.pid, &app.starttime),
		"close exited the process"
	);
	let _ = control("workspace-closed");
	std::thread::sleep(Duration::from_millis(700));
	capture(
		&wid,
		&integration_shots().join("02-copy-cancelled-closed.png"),
	);
	snapshot(app.pid, &app.starttime);
	let _ = fs::remove_file(&hold);

	click(&wid, "btn-open-workspace");
	click(&wid, "workspace-path-input");
	focus(&wid);
	let st = Command::new("xdotool")
		.args([
			"type",
			"--delay",
			"15",
			"--window",
			&wid,
			&ws_b.to_string_lossy(),
		])
		.status()
		.expect("xdotool type");
	assert!(st.success(), "typing the workspace path failed");
	click(&wid, "btn-workspace-open-confirm");
	let opened = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=open path=",
		Duration::from_secs(6),
	);
	assert!(
		opened
			.iter()
			.any(|l| l.contains(&ws_b.to_string_lossy().to_string())),
		"reopen path was not workspace B: {opened:?}"
	);
	lines_until(&app.rx, "[APP:READY_REPOS:", Duration::from_secs(12));
	assert_eq!(clip_get(), sentinel);
	assert!(
		same_proc(app.pid, &app.starttime),
		"reopen changed the process"
	);
	capture(&wid, &integration_shots().join("03-reopened.png"));
	snapshot(app.pid, &app.starttime);

	request_wm_delete(&wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
	assert!(!same_proc(app.pid, &app.starttime));
	assert!(
		descendants(app.pid).is_empty(),
		"owned children survived quit"
	);
}

/// Reads until every pattern has been seen, in any order.
fn lines_until_all(
	rx: &Receiver<String>,
	patterns: &[&str],
	timeout: Duration,
) -> Vec<String> {
	let deadline = Instant::now() + scaled(timeout);
	let mut seen = Vec::new();
	let mut missing: Vec<&str> = patterns.to_vec();
	while Instant::now() < deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		match rx.recv_timeout(remaining.min(Duration::from_millis(40))) {
			Ok(line) => {
				missing.retain(|pattern| !line.contains(pattern));
				seen.push(line);
				if missing.is_empty() {
					return seen;
				}
			}
			Err(RecvTimeoutError::Timeout) => continue,
			Err(RecvTimeoutError::Disconnected) => break,
		}
	}
	panic!("timed out after {timeout:?} waiting for {missing:?}; saw {seen:?}");
}

/// Every line the app prints during `window`.
fn lines_for(rx: &Receiver<String>, window: Duration) -> Vec<String> {
	let deadline = Instant::now() + scaled(window);
	let mut seen = Vec::new();
	while Instant::now() < deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		match rx.recv_timeout(remaining.min(Duration::from_millis(40))) {
			Ok(line) => seen.push(line),
			Err(RecvTimeoutError::Timeout) => continue,
			Err(RecvTimeoutError::Disconnected) => break,
		}
	}
	seen
}

fn position(lines: &[String], pattern: &str) -> Option<usize> {
	lines.iter().position(|l| l.contains(pattern))
}

/// Waits until the app reports the control as no longer drawn.
fn absent(id: &str) {
	let deadline = Instant::now() + scaled(Duration::from_secs(4));
	loop {
		let shown = BOUNDS.with(|slot| {
			slot.borrow().as_ref().is_some_and(|b| {
				snip_native_e2e::lookup_bounds(&b.lock().unwrap(), id).is_some()
			})
		});
		if !shown {
			return;
		}
		assert!(Instant::now() < deadline, "control {id} is still drawn");
		std::thread::sleep(Duration::from_millis(40));
	}
}

fn no_git_children(pid: u32) -> bool {
	descendants(pid).into_iter().all(|child| !git_busy(child))
}

fn open_workspace(wid: &str, path: &Path) {
	click(wid, "btn-open-workspace");
	click(wid, "workspace-path-input");
	focus(wid);
	let st = Command::new("xdotool")
		.args([
			"type",
			"--delay",
			"15",
			"--window",
			wid,
			&path.to_string_lossy(),
		])
		.status()
		.expect("xdotool type");
	assert!(st.success(), "typing the workspace path failed");
	click(wid, "btn-workspace-open-confirm");
}

fn is_drained(line: &str, intent: &str) -> bool {
	line.contains(&format!("phase=drained intent={intent}"))
		&& line.contains("jobs=0")
		&& line.contains("inflight=0")
		&& line.contains("queued=0")
		&& line.contains("leaked=0")
}

fn tree_workspace(git_bin: &Path, root: &Path) -> (PathBuf, PathBuf) {
	let ws = root.join("ws");
	let repo = ws.join("repo");
	init_repo(git_bin, &repo);
	fs::create_dir_all(repo.join("sub")).unwrap();
	fs::write(repo.join("sub").join("nested.txt"), "nested\n").unwrap();
	let dest = root.join("dest");
	fs::create_dir_all(&dest).unwrap();
	(ws, dest)
}

/// A project-tree directory read has no Git child. Close must still wait for
/// the read itself, and the late result must not reach the reopened workspace.
#[test]
fn close_waits_for_a_held_tree_read_and_reopen_ignores_it() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let (ws, dest) = tree_workspace(&git_bin, root.path());
	let hold = root.path().join("tree-hold");
	fs::write(&hold, b"hold").unwrap();

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: Some(&hold),
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	let started = lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:GRAPH_LOADED:",
			"[APP:TREE_IO_HELD]",
		],
		Duration::from_secs(12),
	);
	assert!(
		position(&started, "[APP:TREE_PAGE:").is_none(),
		"the held root read was applied before release: {started:?}"
	);
	wait_git_idle(app.pid, &app.starttime);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	lines_until(
		&app.rx,
		"phase=draining intent=close-workspace",
		Duration::from_secs(4),
	);
	let held = lines_for(&app.rx, Duration::from_millis(1500));
	assert!(
		held.iter().all(|l| {
			!l.contains("phase=drained")
				&& !l.contains("[APP:WORKSPACE: state=closed")
		}),
		"close finished while the tree read was still held: {held:?}"
	);
	assert!(hold.is_file(), "the hold file vanished on its own");
	assert!(
		no_git_children(app.pid),
		"a git child was running; this must be the non-Git read"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"close exited the process"
	);

	fs::remove_file(&hold).unwrap();
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(8),
	);
	let discarded = position(&closed, "[APP:TREE_IO_DISCARDED: stale");
	let drained = closed.iter().position(|l| is_drained(l, "close-workspace"));
	let closed_at = position(&closed, "[APP:WORKSPACE: state=closed");
	assert!(
		discarded.is_some() && drained.is_some(),
		"close did not wait for the tree read: {closed:?}"
	);
	assert!(
		discarded < drained && drained < closed_at,
		"drain was reported before the tree read settled: {closed:?}"
	);
	assert!(
		position(&closed, "[APP:TREE_PAGE:").is_none(),
		"the stale tree read was applied during close: {closed:?}"
	);
	let _ = control("workspace-closed");

	// Same path, same process: only the new read may fill the tree.
	open_workspace(&wid, &ws);
	let opened = lines_until_all(
		&app.rx,
		&[
			"[APP:WORKSPACE: state=open path=",
			"[APP:READY_REPOS: 1]",
			"[APP:TREE_PAGE:",
		],
		Duration::from_secs(12),
	);
	let open_at = position(&opened, "[APP:WORKSPACE: state=open path=");
	let page_at = position(&opened, "[APP:TREE_PAGE:");
	assert!(
		open_at < page_at,
		"a tree page arrived before the workspace reopened: {opened:?}"
	);
	assert!(
		position(&opened, "[APP:TREE_IO_DISCARDED:").is_none(),
		"the reopened workspace discarded its own read: {opened:?}"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"reopen changed the process"
	);

	request_wm_delete(&wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
	assert!(!same_proc(app.pid, &app.starttime));
}

#[test]
fn quit_waits_for_a_held_tree_read() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let (ws, dest) = tree_workspace(&git_bin, root.path());
	let hold = root.path().join("tree-hold");
	fs::write(&hold, b"hold").unwrap();

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: Some(&hold),
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:GRAPH_LOADED:",
			"[APP:TREE_IO_HELD]",
		],
		Duration::from_secs(12),
	);
	wait_git_idle(app.pid, &app.starttime);

	key(&wid, "ctrl+q");
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(4));
	let held = lines_for(&app.rx, Duration::from_millis(1500));
	assert!(
		held.iter().all(|l| !l.contains("phase=drained")),
		"quit drained while the tree read was still held: {held:?}"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"quit exited while the tree read was still held"
	);
	assert!(
		no_git_children(app.pid),
		"a git child was running; this must be the non-Git read"
	);

	fs::remove_file(&hold).unwrap();
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "quit status {status}");
	assert!(!same_proc(app.pid, &app.starttime));
	let mut lines = held;
	while let Ok(line) = app.rx.try_recv() {
		lines.push(line);
	}
	assert!(
		lines.iter().any(|l| is_drained(l, "quit")),
		"missing drained evidence: {lines:?}"
	);
	assert!(
		position(&lines, "[APP:TREE_PAGE:").is_none(),
		"the stale tree read was applied during quit: {lines:?}"
	);
}

fn head_oid(git_bin: &Path, repo: &Path) -> String {
	let out = Command::new(git_bin)
		.args(["rev-parse", "HEAD"])
		.current_dir(repo)
		.output()
		.expect("git rev-parse");
	assert!(out.status.success(), "rev-parse HEAD failed");
	String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn wait_gone(pid: u32, start: &str, what: &str) {
	let deadline = Instant::now() + scaled(Duration::from_secs(6));
	while same_proc(pid, start) {
		assert!(
			Instant::now() < deadline,
			"{what} {pid} survived: {}",
			cmdline(pid)
		);
		std::thread::sleep(Duration::from_millis(30));
	}
}

struct CopyFixture {
	_root: tempfile::TempDir,
	ws: PathBuf,
	repo: PathBuf,
	dest: PathBuf,
	hold: PathBuf,
	wrap_dir: PathBuf,
	git_bin: PathBuf,
}

fn copy_fixture() -> CopyFixture {
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("repo");
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	init_repo(&git_bin, &repo);
	fs::write(repo.join("note.txt"), "copy-bytes\n").unwrap();
	let wrap = root.path().join("bin").join("git");
	hold_all_git_wrapper(&git_bin, &wrap);
	CopyFixture {
		ws,
		repo,
		dest,
		hold: root.path().join("hold"),
		wrap_dir: wrap.parent().unwrap().to_path_buf(),
		git_bin,
		_root: root,
	}
}

fn spawn_copy_app(fx: &CopyFixture) -> (App, String) {
	let app = spawn_app(SpawnOpts {
		workspace: &fx.ws,
		restore: &fx.dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: Some(&fx.hold),
		path_prefix: Some(&fx.wrap_dir),
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:GRAPH_LOADED:",
			"[APP:PREVIEW_LOADED:",
		],
		Duration::from_secs(12),
	);
	wait_git_idle(app.pid, &app.starttime);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	(app, wid)
}

/// Starts a copy that blocks inside its first Git child and returns the child.
fn start_held_copy(
	app: &mut App,
	wid: &str,
	fx: &CopyFixture,
	button: &str,
	prep: &str,
	done: &str,
) -> (u32, String) {
	fs::write(&fx.hold, b"hold").unwrap();
	assert!(
		no_git_children(app.pid),
		"a git child existed before the copy; that would not prove the copy token"
	);
	click(wid, button);
	let lines = lines_until(&app.rx, prep, Duration::from_secs(4));
	assert!(
		position(&lines, done).is_none(),
		"copy finished before its git child was observed: {lines:?}"
	);
	let child = wait_copy_git(app.pid, &app.starttime);
	app.tracked.push(child.clone());
	assert!(
		same_proc(child.0, &child.1),
		"copy git exited before cancel"
	);
	child
}

/// Clicks the cancel button of a held copy and checks nothing was published.
fn cancel_held_copy(
	app: &App,
	wid: &str,
	fx: &CopyFixture,
	child: &(u32, String),
	done: &str,
	sentinel: &str,
) {
	click(wid, "btn-copy-cancel");
	let lines = lines_until(
		&app.rx,
		"[APP:COPY_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert!(
		position(&lines, "[APP:COPY_CANCEL]").is_some(),
		"the cancel button did not reach the model: {lines:?}"
	);
	assert!(
		position(&lines, done).is_none(),
		"cancelled copy still reported done: {lines:?}"
	);
	assert!(
		fx.hold.is_file(),
		"hold file was removed; the child could have finished instead of being cancelled"
	);
	wait_gone(child.0, &child.1, "cancelled git child");
	wait_git_idle(app.pid, &app.starttime);
	absent("btn-copy-cancel");
	assert_eq!(clip_get(), sentinel, "clipboard changed after cancel");
	assert!(same_proc(app.pid, &app.starttime), "cancel exited the app");
}

/// The cancel button itself, for a file copy and then a commit copy.
#[test]
fn copy_cancel_button_stops_file_and_commit_copy() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let (mut app, wid) = spawn_copy_app(&fx);

	let sentinel = "CLIP-SENTINEL-copy-button-file";
	clip_set(sentinel);
	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:FILE_TOGGLED:", Duration::from_secs(4));
	let child = start_held_copy(
		&mut app,
		&wid,
		&fx,
		"btn-copy",
		"[APP:COPY_PREP:",
		"[APP:COPY_DONE:",
	);
	cancel_held_copy(&app, &wid, &fx, &child, "[APP:COPY_DONE:", sentinel);

	// Busy is cleared: the same copy runs to the end once Git is released.
	fs::remove_file(&fx.hold).unwrap();
	click(&wid, "btn-copy");
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(8));
	let copied = clip_get();
	assert!(
		copied.contains("note.txt") && copied.contains("copy-bytes"),
		"the copy after cancel did not publish the file: {copied:?}"
	);
	wait_git_idle(app.pid, &app.starttime);

	let sentinel = "CLIP-SENTINEL-copy-button-commit";
	clip_set(sentinel);
	let sha = head_oid(&fx.git_bin, &fx.repo)[..7].to_string();
	click(&wid, &format!("commit-row:{sha}"));
	lines_until(&app.rx, "[APP:COMMIT_SELECTED:", Duration::from_secs(4));
	lines_until(
		&app.rx,
		"[APP:E2E_PREVIEW: source=commit_diff",
		Duration::from_secs(8),
	);
	wait_git_idle(app.pid, &app.starttime);
	let child = start_held_copy(
		&mut app,
		&wid,
		&fx,
		"btn-copy-commits",
		"[APP:COPY_COMMITS_PREP:",
		"[APP:COPY_COMMITS_DONE:",
	);
	cancel_held_copy(
		&app,
		&wid,
		&fx,
		&child,
		"[APP:COPY_COMMITS_DONE:",
		sentinel,
	);

	fs::remove_file(&fx.hold).unwrap();
	click(&wid, "btn-copy-commits");
	lines_until(&app.rx, "[APP:COPY_COMMITS_DONE:", Duration::from_secs(8));
	assert_ne!(
		clip_get(),
		sentinel,
		"the commit copy after cancel did not publish"
	);

	request_wm_delete(&wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
	assert!(
		descendants(app.pid).is_empty(),
		"owned children survived quit"
	);
}

/// Quit shares the copy token with close, and a commit copy shares it with a
/// file copy.
#[test]
fn quit_cancels_in_flight_commit_copy_without_writing_clipboard() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let (mut app, wid) = spawn_copy_app(&fx);

	let sentinel = "CLIP-SENTINEL-quit-commit-copy";
	clip_set(sentinel);
	let sha = head_oid(&fx.git_bin, &fx.repo)[..7].to_string();
	click(&wid, &format!("commit-row:{sha}"));
	lines_until(&app.rx, "[APP:COMMIT_SELECTED:", Duration::from_secs(4));
	lines_until(
		&app.rx,
		"[APP:E2E_PREVIEW: source=commit_diff",
		Duration::from_secs(8),
	);
	wait_git_idle(app.pid, &app.starttime);
	let child = start_held_copy(
		&mut app,
		&wid,
		&fx,
		"btn-copy-commits",
		"[APP:COPY_COMMITS_PREP:",
		"[APP:COPY_COMMITS_DONE:",
	);

	key(&wid, "ctrl+q");
	let status = wait_exit(&mut app, Duration::from_secs(10));
	assert!(status.success(), "quit status {status}");
	assert!(
		fx.hold.is_file(),
		"hold file was removed; the child could have finished instead of being cancelled"
	);
	assert!(
		!same_proc(child.0, &child.1),
		"git child survived quit: {}",
		cmdline(child.0)
	);
	let mut lines = Vec::new();
	while let Ok(line) = app.rx.try_recv() {
		lines.push(line);
	}
	assert!(
		position(&lines, "[APP:QUIT: deferred]").is_some(),
		"missing deferred quit: {lines:?}"
	);
	assert!(
		position(&lines, "[APP:COPY_DISCARDED:").is_some(),
		"quit did not discard the in-flight commit copy: {lines:?}"
	);
	assert!(
		position(&lines, "[APP:COPY_COMMITS_DONE:").is_none(),
		"cancelled commit copy still reported done: {lines:?}"
	);
	assert!(
		lines.iter().any(|l| is_drained(l, "quit")),
		"missing drained evidence: {lines:?}"
	);
	assert_eq!(clip_get(), sentinel, "clipboard changed after quit");
}

/// A confirmed write is never interrupted: switching workspace is refused
/// with a reason until the write has finished and reported its result.
#[test]
fn open_workspace_is_refused_while_apply_writes() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws_a = root.path().join("ws-a");
	let ws_b = root.path().join("ws-b");
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	init_repo(&git_bin, &ws_a.join("repo-a"));
	init_repo(&git_bin, &ws_b.join("repo-b"));
	fs::write(ws_a.join("repo-a").join("note.txt"), "apply-bytes\n").unwrap();

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws_a,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: Some(6000),
		hold_file: None,
		path_prefix: None,
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:PREVIEW_LOADED:",
		],
		Duration::from_secs(12),
	);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:FILE_TOGGLED:", Duration::from_secs(4));
	click(&wid, "btn-copy");
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(8));
	click(&wid, "btn-paste");
	lines_until(&app.rx, "[APP:PASTE_PREVIEW:", Duration::from_secs(6));
	click(&wid, "btn-apply");
	lines_until(&app.rx, "[APP:PASTE_APPLYING]", Duration::from_secs(4));

	click(&wid, "btn-workspace-menu");
	open_workspace(&wid, &ws_b);
	let refused = lines_until(
		&app.rx,
		"phase=refused intent=open-workspace reason=applying",
		Duration::from_secs(5),
	);
	assert!(
		position(&refused, "[APP:PASTE_DONE:").is_none(),
		"the write had already finished; the refusal proves nothing: {refused:?}"
	);
	assert!(
		position(&refused, "[APP:PASTE_BUSY: refused=open-workspace]")
			.is_some(),
		"the refusal carried no reason: {refused:?}"
	);

	let done =
		lines_until(&app.rx, "[APP:PASTE_DONE:", Duration::from_secs(10));
	assert!(
		done.iter().chain(&refused).all(|l| {
			!l.contains("[APP:WORKSPACE: state=")
				&& !l.contains("phase=draining")
		}),
		"the workspace changed during a confirmed write: {refused:?} {done:?}"
	);
	assert_eq!(
		fs::read(dest.join("note.txt")).expect("apply wrote note.txt"),
		b"apply-bytes",
		"wire format drops the trailing newline"
	);
	assert!(same_proc(app.pid, &app.starttime));

	request_wm_delete(&wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(&mut app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
}

/// Starts a paste preview that blocks inside its first Git child.
fn start_held_paste(
	app: &mut App,
	wid: &str,
	fx: &CopyFixture,
) -> (u32, String) {
	fs::write(&fx.hold, b"hold").unwrap();
	assert!(
		no_git_children(app.pid),
		"a git child existed before the paste; that would not prove the paste token"
	);
	click(wid, "btn-paste");
	let lines =
		lines_until(&app.rx, "[APP:PASTE_LOADING]", Duration::from_secs(4));
	assert!(
		position(&lines, "[APP:PASTE_PREVIEW:").is_none(),
		"the preview was ready before its read started: {lines:?}"
	);
	let child = wait_copy_git(app.pid, &app.starttime);
	app.tracked.push(child.clone());
	child
}

/// After a cancel: the read was dropped, its child is gone, no plan is shown.
fn assert_paste_dropped(
	app: &App,
	fx: &CopyFixture,
	child: &(u32, String),
	lines: &[String],
) {
	assert!(
		position(lines, "[APP:PASTE_DISCARDED: cancelled]").is_some(),
		"the cancelled read was not dropped: {lines:?}"
	);
	assert!(
		position(lines, "[APP:PASTE_PREVIEW:").is_none()
			&& position(lines, "[APP:PASTE_MAPPED:").is_none(),
		"a cancelled read still produced a plan: {lines:?}"
	);
	assert!(
		fx.hold.is_file(),
		"hold file was removed; the child could have finished instead of being cancelled"
	);
	wait_gone(child.0, &child.1, "cancelled paste git child");
	wait_git_idle(app.pid, &app.starttime);
	assert!(same_proc(app.pid, &app.starttime), "cancel exited the app");
}

fn quit_cleanly(app: &mut App, wid: &str) {
	request_wm_delete(wid);
	lines_until(&app.rx, "[APP:QUIT: deferred]", Duration::from_secs(6));
	let status = wait_exit(app, Duration::from_secs(8));
	assert!(status.success(), "window close exited uncleanly: {status}");
	assert!(
		descendants(app.pid).is_empty(),
		"owned children survived quit"
	);
}

/// Cancel button, Escape, and Apply while the first preview is still read.
#[test]
fn paste_preview_cancel_stops_the_read_and_writes_nothing() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let (mut app, wid) = spawn_copy_app(&fx);
	let payload = "// FILE: pasted.txt\npaste-bytes\n";
	clip_set(payload);

	let child = start_held_paste(&mut app, &wid, &fx);
	let _ = control("paste-loading");
	capture(&wid, &integration_shots().join("04-paste-loading.png"));
	// Apply is locked: neither the button nor Enter may act on a plan that
	// does not exist yet.
	click(&wid, "btn-apply");
	key(&wid, "Return");
	let refused = lines_until(
		&app.rx,
		"[APP:APPLY_IGNORED: loading]",
		Duration::from_secs(4),
	);
	assert!(
		position(&refused, "[APP:PASTE_APPLYING]").is_none(),
		"apply ran while the preview was loading: {refused:?}"
	);
	click(&wid, "btn-cancel");
	let lines = lines_until(
		&app.rx,
		"[APP:PASTE_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert!(
		position(&lines, "[APP:PASTE_CANCELLED]").is_some(),
		"the cancel button did not reach the model: {lines:?}"
	);
	assert_paste_dropped(&app, &fx, &child, &lines);
	absent("btn-apply");
	assert!(!fx.dest.join("pasted.txt").exists(), "cancel wrote a file");
	assert_eq!(clip_get(), payload, "clipboard changed after cancel");

	// Escape takes the same path.
	fs::remove_file(&fx.hold).unwrap();
	let child = start_held_paste(&mut app, &wid, &fx);
	let _ = control("paste-loading");
	key(&wid, "Escape");
	let lines = lines_until(
		&app.rx,
		"[APP:PASTE_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert!(
		position(&lines, "[APP:PASTE_CANCELLED]").is_some(),
		"Escape did not cancel the loading preview: {lines:?}"
	);
	assert_paste_dropped(&app, &fx, &child, &lines);
	absent("btn-apply");
	assert!(!fx.dest.join("pasted.txt").exists(), "Escape wrote a file");

	// The same paste still works once Git is released.
	fs::remove_file(&fx.hold).unwrap();
	click(&wid, "btn-paste");
	lines_until(
		&app.rx,
		"[APP:PASTE_PREVIEW: items=1",
		Duration::from_secs(8),
	);
	click(&wid, "btn-apply");
	lines_until(&app.rx, "[APP:PASTE_DONE:", Duration::from_secs(8));
	assert_eq!(
		fs::read(fx.dest.join("pasted.txt")).expect("apply wrote the file"),
		b"paste-bytes"
	);
	quit_cleanly(&mut app, &wid);
}

/// Both raw and plan-amplified overflow invalidate a previously armed restore.
#[test]
fn paste_budget_refusal_disarms_the_previous_plan() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let protected = fx.dest.join("protected.txt");
	let would_create = fx.dest.join("would-create.txt");
	let sentinel = b"keep these destination bytes\n";
	fs::write(&protected, sentinel).unwrap();
	let (mut app, wid) = spawn_copy_app(&fx);
	click(&wid, "btn-locale");
	lines_until(&app.rx, "[APP:LOCALE: En]", Duration::from_secs(4));

	for (case, raw_bytes) in [
		("raw", 32 * 1024 * 1024 + 1),
		("amplified", 12 * 1024 * 1024),
	] {
		clip_set("// FILE: protected.txt\nreplacement\n// FILE: would-create.txt\nnew bytes\n");
		click(&wid, "btn-paste");
		lines_until(
			&app.rx,
			"[APP:PASTE_PREVIEW: items=2",
			Duration::from_secs(8),
		);
		click(&wid, "paste-overwrite:protected.txt");
		let armed =
			lines_until(&app.rx, "[APP:PASTE_TOGGLED:", Duration::from_secs(4));
		assert!(armed.last().unwrap().contains("state=true"));
		let _ = control("btn-apply");

		let mut payload = String::from("// FILE: oversized.txt\n");
		payload.extend(std::iter::repeat_n('x', raw_bytes - payload.len()));
		assert_eq!(payload.len(), raw_bytes);
		clip_set(&payload);
		click(&wid, "btn-paste");
		let refused = lines_until(
			&app.rx,
			"[APP:PASTE_ERR: preview_memory_limit]",
			Duration::from_secs(15),
		);
		assert!(position(&refused, "[APP:PASTE_PLAN_CLEARED]").is_some());
		assert_eq!(
			position(&refused, "[APP:PASTE_LOADING]").is_some(),
			case == "amplified",
			"{case} did not exercise its expected admission path: {refused:?}"
		);
		absent("btn-apply");
		absent("paste-row:protected.txt");
		let _ = control("status-bar");
		capture(
			&wid,
			&integration_shots().join(format!("paste-budget-{case}.png")),
		);
		key(&wid, "Return");
		let settled = lines_for(&app.rx, Duration::from_millis(500));
		assert!(refused.iter().chain(&settled).all(|line| {
			!line.contains("[APP:PASTE_APPLYING]")
				&& !line.contains("[APP:PASTE_DONE:")
				&& !line.contains("[APP:PASTE_PREVIEW:")
		}));
		assert_eq!(fs::read(&protected).unwrap(), sentinel);
		assert!(!would_create.exists(), "{case} applied the stale create");
		assert!(!fx.dest.join("oversized.txt").exists());
		assert_eq!(clip_get(), payload, "{case} modified the clipboard");
	}
	// A prefix-only shell fits; resolving it amplifies the plan past32MiB.
	// Its destinations are all chosen, but it has no executable import plan.
	let mapped_payload = format!(
		"// FILE: lib/oversized.txt\n{}",
		"x".repeat(12 * 1024 * 1024)
	);
	clip_set(&mapped_payload);
	click(&wid, "btn-paste");
	lines_until(
		&app.rx,
		"[APP:PASTE_PREVIEW: items=0",
		Duration::from_secs(8),
	);
	click(&wid, "paste-map-keep:lib");
	lines_until(
		&app.rx,
		"[APP:PASTE_ERR: preview_memory_limit]",
		Duration::from_secs(15),
	);
	let _ = control("btn-apply"); // visible but disabled; no click handler
	capture(&wid, &integration_shots().join("paste-budget-remap.png"));
	click(&wid, "btn-apply");
	key(&wid, "Return");
	let ignored = lines_until(
		&app.rx,
		"[APP:APPLY_IGNORED: no_plan]",
		Duration::from_secs(4),
	);
	assert!(ignored
		.iter()
		.all(|line| !line.contains("[APP:PASTE_APPLYING]")
			&& !line.contains("[APP:PASTE_DONE:")));
	assert_eq!(fs::read(&protected).unwrap(), sentinel);
	assert!(!would_create.exists());
	assert!(!fx.dest.join("lib").exists());
	assert_eq!(clip_get(), mapped_payload);

	quit_cleanly(&mut app, &wid);
}

/// A second paste while the first is still read: only the newer one lands.
#[test]
fn newer_paste_replaces_the_one_still_loading() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let (mut app, wid) = spawn_copy_app(&fx);

	clip_set("// FILE: first.txt\nfirst-bytes\n");
	let first = start_held_paste(&mut app, &wid, &fx);
	clip_set("// FILE: second.txt\nsecond-bytes\n");
	click(&wid, "btn-paste");
	let lines = lines_until(
		&app.rx,
		"[APP:PASTE_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert!(
		position(&lines, "[APP:PASTE_LOADING]").is_some(),
		"the second paste did not start: {lines:?}"
	);
	assert!(
		position(&lines, "[APP:PASTE_PREVIEW:").is_none(),
		"a preview appeared while Git was held: {lines:?}"
	);
	wait_gone(first.0, &first.1, "superseded paste git child");
	assert!(fx.hold.is_file());

	// Clipboard changes without another Paste must not replace the captured intent.
	let after_gesture = "// FILE: never-pasted.txt\nnot requested\n";
	clip_set(after_gesture);
	fs::remove_file(&fx.hold).unwrap();
	let ready = lines_until(
		&app.rx,
		"[APP:PASTE_PREVIEW: items=1",
		Duration::from_secs(8),
	);
	assert_eq!(
		ready
			.iter()
			.filter(|l| l.contains("[APP:PASTE_PREVIEW:"))
			.count(),
		1,
		"more than one preview landed: {ready:?}"
	);
	let _ = control("paste-row:second.txt");
	absent("paste-row:first.txt");
	click(&wid, "btn-apply");
	lines_until(&app.rx, "[APP:PASTE_DONE:", Duration::from_secs(8));
	assert_eq!(
		fs::read(fx.dest.join("second.txt")).expect("apply wrote second"),
		b"second-bytes"
	);
	assert!(
		!fx.dest.join("first.txt").exists(),
		"the superseded paste was written"
	);
	assert!(!fx.dest.join("never-pasted.txt").exists());
	assert_eq!(clip_get(), after_gesture);
	quit_cleanly(&mut app, &wid);
}

/// A mapping change replans in the background. The old writes are gone as
/// soon as the choice is made, and a second choice replaces the first.
#[test]
fn mapping_change_replans_and_cannot_apply_the_old_plan() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = copy_fixture();
	let (mut app, wid) = spawn_copy_app(&fx);

	clip_set("// FILE: lib/mapped.txt\nmapped-bytes\n");
	click(&wid, "btn-paste");
	let ready =
		lines_until(&app.rx, "[APP:PASTE_PREVIEW:", Duration::from_secs(8));
	let repo_text = fx.repo.to_string_lossy().to_string();
	let repo_idx = ready
		.iter()
		.find_map(|l| {
			let rest = l
				.split("[APP:PASTE_MAP_CANDIDATE: prefix=lib idx=")
				.nth(1)?;
			let (idx, path) = rest.split_once(" path=")?;
			(path.trim_end_matches(']') == repo_text).then(|| idx.to_string())
		})
		.unwrap_or_else(|| panic!("repo is not a candidate: {ready:?}"));
	wait_git_idle(app.pid, &app.starttime);

	fs::write(&fx.hold, b"hold").unwrap();
	click(&wid, "paste-map-keep:lib");
	lines_until(&app.rx, "[APP:PASTE_LOADING]", Duration::from_secs(4));
	let first = wait_copy_git(app.pid, &app.starttime);
	app.tracked.push(first.clone());
	let _ = control("paste-loading");
	key(&wid, "Return");
	let refused = lines_until(
		&app.rx,
		"[APP:APPLY_IGNORED: loading]",
		Duration::from_secs(4),
	);
	assert!(
		position(&refused, "[APP:PASTE_APPLYING]").is_none(),
		"apply ran while the mapping was being replanned: {refused:?}"
	);

	click(&wid, &format!("paste-map-pick:lib:{repo_idx}"));
	let lines = lines_until(
		&app.rx,
		"[APP:PASTE_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert!(
		position(&lines, "[APP:PASTE_MAPPED:").is_none(),
		"the superseded mapping still landed: {lines:?}"
	);
	wait_gone(first.0, &first.1, "superseded mapping git child");

	fs::remove_file(&fx.hold).unwrap();
	let mapped =
		lines_until(&app.rx, "[APP:PASTE_MAPPED:", Duration::from_secs(8));
	let line = mapped.last().unwrap();
	assert!(
		line.contains(&format!("prefix=lib dest={repo_text} items=1")),
		"the landed mapping is not the second choice: {line}"
	);
	click(&wid, "btn-apply");
	lines_until(&app.rx, "[APP:PASTE_DONE:", Duration::from_secs(8));
	assert_eq!(
		fs::read(fx.repo.join("mapped.txt")).expect("apply wrote the file"),
		b"mapped-bytes"
	);
	assert!(
		!fx.dest.join("lib").exists(),
		"the first mapping choice was written"
	);
	quit_cleanly(&mut app, &wid);
}

fn repo_state(git_bin: &Path, repo: &Path) -> String {
	let mut out = String::new();
	for args in [
		vec!["rev-parse", "HEAD"],
		vec!["for-each-ref"],
		vec!["status", "--porcelain=v1", "--untracked-files=all"],
		vec!["ls-files", "--stage"],
	] {
		let res = Command::new(git_bin)
			.args(&args)
			.current_dir(repo)
			.output()
			.expect("git");
		assert!(res.status.success(), "git {args:?} failed");
		out.push_str(&String::from_utf8_lossy(&res.stdout));
		out.push('\n');
	}
	out
}

/// A commit preview reads the destination repository. Cancelling it and
/// closing the workspace over it must leave HEAD, refs and the index alone.
#[test]
fn commit_preview_cancel_and_close_leave_the_destination_untouched() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let mut fx = copy_fixture();
	let dest_repo = fx.dest.join("dest-repo");
	init_repo(&fx.git_bin, &dest_repo);
	fx.dest = dest_repo.clone();
	// A second commit so the payload is not the destination's own history.
	git(&fx.git_bin, &fx.repo, &["add", "note.txt"]);
	git(&fx.git_bin, &fx.repo, &["commit", "-q", "-m", "add note"]);
	// Startup waits for a preview, so keep one change in the working tree.
	fs::write(fx.repo.join("extra.txt"), "extra\n").unwrap();
	let (mut app, wid) = spawn_copy_app(&fx);

	let sha = head_oid(&fx.git_bin, &fx.repo)[..7].to_string();
	click(&wid, &format!("commit-row:{sha}"));
	lines_until(&app.rx, "[APP:COMMIT_SELECTED:", Duration::from_secs(4));
	lines_until(
		&app.rx,
		"[APP:E2E_PREVIEW: source=commit_diff",
		Duration::from_secs(8),
	);
	click(&wid, "btn-copy-commits");
	lines_until(&app.rx, "[APP:COPY_COMMITS_DONE:", Duration::from_secs(8));
	wait_git_idle(app.pid, &app.starttime);
	let payload = clip_get();
	let before = repo_state(&fx.git_bin, &dest_repo);

	let child = start_held_paste(&mut app, &wid, &fx);
	click(&wid, "btn-cancel");
	let lines = lines_until(
		&app.rx,
		"[APP:PASTE_DISCARDED: cancelled]",
		Duration::from_secs(8),
	);
	assert_paste_dropped(&app, &fx, &child, &lines);
	absent("btn-apply");
	assert_eq!(repo_state(&fx.git_bin, &dest_repo), before);
	assert_eq!(clip_get(), payload, "clipboard changed after cancel");

	// Released, the same commit preview is built off the UI thread.
	fs::remove_file(&fx.hold).unwrap();
	click(&wid, "btn-paste");
	lines_until(&app.rx, "[APP:PASTE_PREVIEW:", Duration::from_secs(8));
	let _ = control("paste-commit-whole");
	click(&wid, "btn-cancel");
	lines_until(&app.rx, "[APP:PASTE_CANCELLED]", Duration::from_secs(4));
	wait_git_idle(app.pid, &app.starttime);

	// Close while a preview is still being read.
	let child = start_held_paste(&mut app, &wid, &fx);
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(12),
	);
	assert!(
		position(&closed, "[APP:PASTE_DISCARDED:").is_some(),
		"close did not drop the loading preview: {closed:?}"
	);
	assert!(
		position(&closed, "[APP:PASTE_PREVIEW:").is_none(),
		"a preview landed during close: {closed:?}"
	);
	assert!(
		closed.iter().any(|l| is_drained(l, "close-workspace")),
		"close did not report a real drain: {closed:?}"
	);
	assert!(fx.hold.is_file());
	wait_gone(child.0, &child.1, "paste git child after close");
	let _ = control("workspace-closed");
	absent("btn-apply");
	assert_eq!(repo_state(&fx.git_bin, &dest_repo), before);
	assert_eq!(clip_get(), payload, "clipboard changed across close");

	// Reopened in the same process: the dropped preview does not come back.
	fs::remove_file(&fx.hold).unwrap();
	open_workspace(&wid, &fx.ws);
	let opened = lines_until_all(
		&app.rx,
		&["[APP:WORKSPACE: state=open path=", "[APP:READY_REPOS: 1]"],
		Duration::from_secs(12),
	);
	let settle = lines_for(&app.rx, Duration::from_millis(800));
	assert!(
		opened
			.iter()
			.chain(&settle)
			.all(|l| !l.contains("[APP:PASTE_PREVIEW:")),
		"the old preview appeared in the reopened workspace: {opened:?} {settle:?}"
	);
	absent("btn-apply");
	assert_eq!(repo_state(&fx.git_bin, &dest_repo), before);
	assert!(same_proc(app.pid, &app.starttime));
	quit_cleanly(&mut app, &wid);
}

/// Opening and loading a historical revision tree must not cancel an active or
/// queued project-tree directory worker.
#[test]
fn project_tree_finishes_while_historical_tree_loads() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("repo");
	init_repo(&git_bin, &repo);
	fs::write(repo.join("file.txt"), "first commit\n").unwrap();
	git(&git_bin, &repo, &["add", "file.txt"]);
	git(&git_bin, &repo, &["commit", "-q", "-m", "init commit"]);

	// Add subfolders and second commit
	fs::create_dir_all(repo.join("sub")).unwrap();
	fs::write(repo.join("sub").join("nested.txt"), "nested\n").unwrap();
	fs::create_dir_all(repo.join("sub2")).unwrap();
	fs::write(repo.join("sub2").join("nested2.txt"), "nested2\n").unwrap();
	fs::write(repo.join("extra.txt"), "working extra\n").unwrap();
	git(&git_bin, &repo, &["add", "sub", "sub2"]);
	git(&git_bin, &repo, &["commit", "-q", "-m", "add sub"]);

	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();

	let hold = root.path().join("tree-hold");

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: Some(&hold),
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	// Wait for app ready
	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:GRAPH_LOADED:",
			"[APP:TREE_PAGE:",
		],
		Duration::from_secs(12),
	);

	// Switch to Project (FileExplorer) tab
	click(&wid, "rail-project");
	lines_until(
		&app.rx,
		"[APP:TAB_SWITCHED: FileExplorer",
		Duration::from_secs(4),
	);

	// Start project tree expansion for "sub". Set hold file so read holds at hold_tree_read.
	fs::write(&hold, b"hold").unwrap();
	click(&wid, "tree-row:sub");
	lines_until(&app.rx, "[APP:TREE_IO_HELD]", Duration::from_secs(6));

	// While first read is held in background, queue second expansion for "sub2"
	click(&wid, "tree-row:sub2");

	// Select commit and browse historical tree (triggers load_rev_dir)
	let sha = head_oid(&git_bin, &repo);
	click(&wid, &format!("commit-row:{}", &sha[..7]));
	lines_until(&app.rx, "[APP:COMMIT_SELECTED:", Duration::from_secs(6));
	lines_until(
		&app.rx,
		&format!("[APP:E2E_PREVIEW: source=commit_diff rev={sha} "),
		Duration::from_secs(6),
	);
	click(&wid, &format!("btn-browse-tree:{sha}"));
	lines_until(&app.rx, "[APP:REV_TREE:", Duration::from_secs(6));

	// Release the project tree hold
	fs::remove_file(&hold).unwrap();

	// Under prior bug (history.rs using self.tree_cancel), load_rev_dir cancelled tree_cancel,
	// so the project tree worker saw is_cancelled() after finishing "sub" and broke out,
	// leaving queued "sub2" stranded!
	// Under our fix (separate rev_tree_cancel), both "sub" and queued "sub2" complete successfully!
	lines_until(&app.rx, "[APP:TREE_PAGE: rel=sub", Duration::from_secs(8));
	lines_until(&app.rx, "[APP:TREE_PAGE: rel=sub2", Duration::from_secs(8));

	// Leave rev tree and verify app clean exit
	click(&wid, "btn-leave-tree");
	lines_until(&app.rx, "[APP:REV_TREE: off]", Duration::from_secs(4));

	quit_cleanly(&mut app, &wid);
}

/// When a drain times out after >8s deadline, the workspace remains open and recovers:
/// stale late results are discarded, tree cancel token is rearmed, loading flags cleared,
/// basket selection is preserved, new directory expansion and preview succeed, and
/// subsequent close drains cleanly.
#[test]
fn failed_drain_recovers_and_allows_expand_preview_and_close() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("repo");
	init_repo(&git_bin, &repo);

	fs::write(repo.join("note.txt"), "important note\n").unwrap();
	fs::create_dir_all(repo.join("sub")).unwrap();
	fs::write(repo.join("sub").join("nested.txt"), "nested content\n").unwrap();
	fs::create_dir_all(repo.join("sub2")).unwrap();
	fs::write(repo.join("sub2").join("nested2.txt"), "nested2 content\n")
		.unwrap();
	git(&git_bin, &repo, &["add", "."]);
	git(&git_bin, &repo, &["commit", "-q", "-m", "init"]);
	// Keep one working change for preview ready
	fs::write(repo.join("extra.txt"), "extra\n").unwrap();

	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();

	let hold = root.path().join("tree-hold");

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: Some(&hold),
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:TREE_PAGE:",
			"[APP:PREVIEW_LOADED:",
		],
		Duration::from_secs(12),
	);

	// Switch to Project (FileExplorer) tab
	click(&wid, "rail-project");
	lines_until(
		&app.rx,
		"[APP:TAB_SWITCHED: FileExplorer",
		Duration::from_secs(4),
	);

	// Select note.txt into basket so we can prove selection survives failed drain
	ctrl_click(&wid, "tree-row:note.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));

	// Hold the directory expansion of "sub"
	fs::write(&hold, b"hold").unwrap();
	click(&wid, "tree-row:sub");
	lines_until(&app.rx, "[APP:TREE_IO_HELD]", Duration::from_secs(6));

	// Pre-drain queued read on the alive worker: click "sub2"
	click(&wid, "tree-row:sub2");

	// User requests Close Workspace while directory read is held
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	lines_until(
		&app.rx,
		"phase=draining intent=close-workspace",
		Duration::from_secs(4),
	);

	// Keep hold on disk for >8s drain deadline: drain must timeout and fail
	let failed_lines = lines_until(
		&app.rx,
		"phase=failed intent=close-workspace reason=timeout",
		Duration::from_secs(14),
	);
	assert!(
		failed_lines.iter().any(|l| l
			.contains("phase=failed intent=close-workspace reason=timeout")),
		"drain did not fail on >8s timeout: {failed_lines:?}"
	);
	assert!(
		same_proc(app.pid, &app.starttime),
		"app must stay alive on failed drain"
	);

	// Click fresh expansion AFTER phase=failed but BEFORE releasing old hold!
	// Under prior bug, tree_worker_alive was left true so this would queue onto
	// the canceled dead worker and be stranded when the hold released.
	click(&wid, "tree-row:sub");

	// Now release hold file so background thread unblocks
	fs::remove_file(&hold).unwrap();

	// The stale directory read must be discarded and the fresh directory
	// expansion must arrive with NO second click (concurrent workers may finish in any order).
	let tree_lines = lines_until_all(
		&app.rx,
		&["[APP:TREE_IO_DISCARDED: stale", "[APP:TREE_PAGE: rel=sub"],
		Duration::from_secs(8),
	);
	assert!(
		tree_lines
			.iter()
			.any(|l| l.contains("[APP:TREE_IO_DISCARDED: stale")),
		"stale late tree result was not discarded: {tree_lines:?}"
	);
	assert!(
		tree_lines
			.iter()
			.any(|l| l.contains("[APP:TREE_PAGE: rel=sub")),
		"fresh directory expansion was not received: {tree_lines:?}"
	);

	// Basket summary must still report 1 item
	let summary = control("basket-summary");
	assert!(summary[2] > 0, "basket summary must be visible");

	click(&wid, "tree-row:note.txt");
	lines_until(
		&app.rx,
		"[APP:PREVIEW_LOADED: note.txt]",
		Duration::from_secs(6),
	);

	// Second Close Workspace with no holds: drains cleanly and closes!
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(8),
	);
	assert!(
		closed.iter().any(|l| is_drained(l, "close-workspace")),
		"close did not report real drain after recovery: {closed:?}"
	);
	let _ = control("workspace-closed");

	quit_cleanly(&mut app, &wid);
}

/// When a source file is mutated externally after ExportPlan planning
/// but before final revalidation, the export fails with stale_source diagnostic,
/// sentinel clipboard is preserved, and app drains normally.
#[test]
fn export_refuses_when_source_mutated_after_plan_ready() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("repo");
	init_repo(&git_bin, &repo);

	fs::write(repo.join("note.txt"), "initial content\n").unwrap();
	git(&git_bin, &repo, &["add", "."]);
	git(&git_bin, &repo, &["commit", "-q", "-m", "init"]);

	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();

	let sentinel = "SENTINEL_CLIPBOARD_DO_NOT_OVERWRITE";
	clip_set(sentinel);
	assert_eq!(clip_get(), sentinel);

	let hold = root.path().join("export-hold");
	fs::write(&hold, b"hold").unwrap();

	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: None,
		export_hold: Some(&hold),
	});
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();

	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 1]",
			"[APP:REPO_LOADED:",
			"[APP:TREE_PAGE:",
		],
		Duration::from_secs(12),
	);

	// Switch to Project (FileExplorer) tab
	click(&wid, "rail-project");
	lines_until(
		&app.rx,
		"[APP:TAB_SWITCHED: FileExplorer",
		Duration::from_secs(4),
	);

	// Select note.txt into basket
	ctrl_click(&wid, "tree-row:note.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));

	// Trigger copy
	click(&wid, "btn-copy");

	// Wait for plan ready marker
	lines_until(
		&app.rx,
		"[APP:EXPORT_PLAN_READY: files=1]",
		Duration::from_secs(6),
	);

	// Mutate source file on disk while copy is held
	fs::write(repo.join("note.txt"), "mutated content externally\n").unwrap();

	// Release hold barrier
	fs::remove_file(&hold).unwrap();

	// App must emit stale_source diagnostic and transition back to idle
	lines_until(
		&app.rx,
		"[APP:COPY_FAILED: stale_source]",
		Duration::from_secs(6),
	);
	lines_until(&app.rx, "[APP:COPY_IDLE]", Duration::from_secs(4));

	// Clipboard MUST remain untouched sentinel
	assert_eq!(
		clip_get(),
		sentinel,
		"sentinel clipboard must not be overwritten on stale source failure"
	);

	// Close cleanly
	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	let closed = lines_until(
		&app.rx,
		"[APP:WORKSPACE: state=closed",
		Duration::from_secs(8),
	);
	assert!(
		closed.iter().any(|l| is_drained(l, "close-workspace")),
		"close did not report real drain: {closed:?}"
	);
	let _ = control("workspace-closed");

	quit_cleanly(&mut app, &wid);
}

fn request_wm_delete(wid: &str) {
	let dir = std::env::temp_dir().join("snip-native-lifecycle-wm-delete");
	let _ = fs::create_dir_all(&dir);
	let src = dir.join("wm_delete.c");
	let bin = dir.join("wm_delete");
	fs::write(
		&src,
		r#"
#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
	if (argc != 2) return 2;
	unsigned long wid = strtoul(argv[1], NULL, 0);
	Display *d = XOpenDisplay(NULL);
	if (!d) return 3;
	Atom proto = XInternAtom(d, "WM_PROTOCOLS", False);
	Atom del = XInternAtom(d, "WM_DELETE_WINDOW", False);
	XEvent ev;
	memset(&ev, 0, sizeof ev);
	ev.xclient.type = ClientMessage;
	ev.xclient.window = (Window)wid;
	ev.xclient.message_type = proto;
	ev.xclient.format = 32;
	ev.xclient.data.l[0] = (long)del;
	ev.xclient.data.l[1] = CurrentTime;
	Status s = XSendEvent(d, (Window)wid, False, NoEventMask, &ev);
	XFlush(d);
	XCloseDisplay(d);
	return s ? 0 : 4;
}
"#,
	)
	.unwrap();
	let compile = Command::new("cc")
		.args(["-O2", "-o"])
		.arg(&bin)
		.arg(&src)
		.arg("-lX11")
		.status()
		.expect("cc");
	assert!(compile.success(), "could not compile the WM_DELETE helper");
	let st = Command::new(&bin).arg(wid).status().expect("wm_delete");
	assert!(st.success(), "WM_DELETE_WINDOW was not delivered to {wid}");
}

fn wait_git_child(app_pid: u32, app_start: &str) -> (u32, String) {
	let deadline = Instant::now() + scaled(Duration::from_secs(6));
	while Instant::now() < deadline {
		assert!(
			same_proc(app_pid, app_start),
			"app died before the git child appeared"
		);
		for pid in descendants(app_pid) {
			let cmd = cmdline(pid);
			if cmd.contains("sleep")
				|| cmd.ends_with(" git")
				|| cmd.contains("/git ")
				|| cmd.contains(" git ")
			{
				if let Some(start) = starttime(pid) {
					return (pid, start);
				}
			}
		}
		std::thread::sleep(Duration::from_millis(30));
	}
	let kids: Vec<_> = descendants(app_pid)
		.into_iter()
		.map(|pid| format!("{pid}:{}", cmdline(pid)))
		.collect();
	panic!("no held git child appeared; children={kids:?}");
}

/// Two repos `a` and `b`, each with an untracked `note.txt`, and a Git
/// wrapper that blocks while `hold` exists.
struct TwoRepos {
	_root: tempfile::TempDir,
	ws: PathBuf,
	dest: PathBuf,
	hold: PathBuf,
	wrap_dir: PathBuf,
}

fn two_repos() -> TwoRepos {
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	for name in ["a", "b"] {
		let repo = ws.join(name);
		init_repo(&git_bin, &repo);
		fs::write(repo.join("note.txt"), format!("note from {name}\n"))
			.unwrap();
	}
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	let wrap = root.path().join("bin").join("git");
	hold_all_git_wrapper(&git_bin, &wrap);
	TwoRepos {
		ws,
		dest,
		hold: root.path().join("hold"),
		wrap_dir: wrap.parent().unwrap().to_path_buf(),
		_root: root,
	}
}

fn spawn_two(fx: &TwoRepos, tree_hold: Option<&Path>) -> (App, String) {
	let app = spawn_app(SpawnOpts {
		workspace: &fx.ws,
		restore: &fx.dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: Some(&fx.hold),
		path_prefix: Some(&fx.wrap_dir),
		tree_hold,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	lines_until_all(
		&app.rx,
		&[
			"[APP:READY_REPOS: 2]",
			"[APP:REPO_LOADED: a files=1]",
			"[APP:PREVIEW_LOADED: note.txt]",
		],
		Duration::from_secs(12),
	);
	wait_git_idle(app.pid, &app.starttime);
	// Changes starts with every repo node collapsed; the tests work in `a`.
	click(&wid, "change-repo:unstaged:a");
	lines_until(
		&app.rx,
		"[APP:REPO_CHANGES_COLLAPSED: unstaged a collapsed=false]",
		Duration::from_secs(4),
	);
	(app, wid)
}

/// Selects repo `name` (at list index `idx`) from the Project tool.
fn switch_repo(app: &App, wid: &str, name: &str, idx: usize) {
	// The rail button toggles; the shortcut only shows.
	key(wid, "alt+1");
	click(wid, &format!("repo-row:{name}"));
	lines_until(
		&app.rx,
		&format!("[APP:REPO_SELECTING: {idx} ({name})"),
		Duration::from_secs(4),
	);
}

/// Refresh re-reads the open repo's Changes (keeping the opened file), and a
/// repo deleted on disk is dropped instead of leaving its view under
/// another repo's name.
#[test]
fn refresh_reloads_the_open_repo_and_releases_a_vanished_one() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = two_repos();
	let (mut app, wid) = spawn_two(&fx, None);

	// `extra.txt` sorts first; the reload must keep `note.txt` open.
	fs::write(fx.ws.join("a").join("extra.txt"), "extra\n").unwrap();
	key(&wid, "ctrl+r");
	lines_until_all(
		&app.rx,
		&["[APP:READY_REPOS: 2]", "[APP:REPO_LOADED: a files=2]"],
		Duration::from_secs(12),
	);
	let after =
		lines_until(&app.rx, "[APP:PREVIEW_LOADED:", Duration::from_secs(6));
	assert!(
		after
			.last()
			.unwrap()
			.contains("[APP:PREVIEW_LOADED: note.txt]"),
		"refresh lost the opened file: {after:?}"
	);
	control("change-row:extra.txt");

	// A selection in the repo about to vanish must not block a later Copy.
	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));

	fs::remove_dir_all(fx.ws.join("a")).unwrap();
	key(&wid, "ctrl+r");
	let lines = lines_until_all(
		&app.rx,
		&["[APP:REPO_VANISHED: a]", "[APP:READY_REPOS: 1]"],
		Duration::from_secs(12),
	);
	assert!(
		position(&lines, "[APP:REPO_SELECTING:").is_none(),
		"the vanished repo's view was handed to another repo: {lines:?}"
	);
	assert!(
		position(&lines, "[APP:BASKET: n=0").is_some(),
		"the vanished repo's selections stayed in the basket: {lines:?}"
	);
	absent("change-row:note.txt");
	absent("change-row:extra.txt");
	// The log shows the workspace: it reloads over the repo left.
	let graph =
		lines_until(&app.rx, "[APP:GRAPH_LOADED:", Duration::from_secs(8));
	assert!(
		!graph.last().unwrap().contains("commits=0]"),
		"the log stayed empty after a repo vanished: {graph:?}"
	);

	switch_repo(&app, &wid, "b", 0);
	lines_until(&app.rx, "[APP:REPO_LOADED: b", Duration::from_secs(8));
	key(&wid, "alt+0");
	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));
	let sentinel = "CLIP-SENTINEL-vanished-repo";
	clip_set(sentinel);
	click(&wid, "btn-copy");
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(10));
	let copied = clip_get();
	assert!(
		copied.contains("note from b") && !copied.contains("note from a"),
		"copy after a repo vanished: {copied:?}"
	);
	quit_cleanly(&mut app, &wid);
}

/// While the Changes list is still loading, Copy and Select All must not
/// drop that repo's Git-source selections.
#[test]
fn unloaded_change_list_keeps_git_selections() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = two_repos();
	let (mut app, wid) = spawn_two(&fx, None);
	click(&wid, "change-chk:note.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));

	switch_repo(&app, &wid, "b", 1);
	lines_until(&app.rx, "[APP:REPO_LOADED: b", Duration::from_secs(8));
	wait_git_idle(app.pid, &app.starttime);

	// `a`'s status read blocks, so its Changes list stays unloaded.
	fs::write(&fx.hold, b"hold").unwrap();
	switch_repo(&app, &wid, "a", 0);
	app.tracked.push(wait_git_child(app.pid, &app.starttime));

	key(&wid, "alt+s");
	let lines = lines_until(
		&app.rx,
		"[APP:FILES_SELECTED_ALL]",
		Duration::from_secs(4),
	);
	let basket = lines.iter().rfind(|l| l.contains("[APP:BASKET:")).unwrap();
	assert!(
		basket.contains("a untracked note.txt"),
		"Select All while loading dropped the Git selection: {basket}"
	);
	key(&wid, "alt+d");
	lines_until(&app.rx, "[APP:FILES_DESELECTED]", Duration::from_secs(4));

	let sentinel = "CLIP-SENTINEL-unloaded-changes";
	clip_set(sentinel);
	click(&wid, "btn-copy");
	let lines = lines_until(&app.rx, "[APP:BASKET:", Duration::from_secs(4));
	let basket = lines.last().unwrap();
	assert!(
		basket.contains("n=1") && basket.contains("a untracked note.txt"),
		"Copy while loading dropped the Git selection: {lines:?}"
	);
	fs::remove_file(&fx.hold).unwrap();
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(10));
	let copied = clip_get();
	assert!(
		copied.contains("note.txt") && copied.contains("note from a"),
		"the copy lost the selected change: {copied:?}"
	);
	wait_git_idle(app.pid, &app.starttime);
	quit_cleanly(&mut app, &wid);
}

/// A file name that is not UTF-8 is shown but cannot be checked, so the other
/// selected changes still copy.
#[test]
fn non_utf8_change_is_not_checkable_and_does_not_block_copy() {
	use std::os::unix::ffi::OsStrExt;
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let git_bin = real_git();
	let root = tempfile::tempdir().unwrap();
	let ws = root.path().join("ws");
	let repo = ws.join("e");
	init_repo(&git_bin, &repo);
	let bad = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
	fs::write(repo.join(bad), "bad\n").unwrap();
	fs::write(repo.join("good.txt"), "good bytes\n").unwrap();
	let dest = root.path().join("dest");
	fs::create_dir_all(&dest).unwrap();
	let mut app = spawn_app(SpawnOpts {
		workspace: &ws,
		restore: &dest,
		read_delay_ms: None,
		apply_delay_ms: None,
		hold_file: None,
		path_prefix: None,
		tree_hold: None,
		export_hold: None,
	});
	let wid = find_wid(app.pid);
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	lines_until_all(
		&app.rx,
		&["[APP:READY_REPOS: 1]", "[APP:REPO_LOADED: e files=2]"],
		Duration::from_secs(12),
	);

	// Files are path-sorted; the lossy `bad\u{FFFD}.txt` is index 0.
	click(&wid, "change-chk-invalid:0");
	let lines = lines_for(&app.rx, Duration::from_millis(800));
	assert!(
		position(&lines, "[APP:FILE_TOGGLED:").is_none(),
		"a non-UTF-8 change was selectable: {lines:?}"
	);
	click(&wid, "change-chk:good.txt");
	lines_until(&app.rx, "[APP:BASKET: n=1", Duration::from_secs(4));

	let sentinel = "CLIP-SENTINEL-non-utf8";
	clip_set(sentinel);
	click(&wid, "btn-copy");
	lines_until(&app.rx, "[APP:COPY_DONE:", Duration::from_secs(10));
	let copied = clip_get();
	assert!(
		copied.contains("good.txt") && copied.contains("good bytes"),
		"copy did not publish the valid change: {copied:?}"
	);
	quit_cleanly(&mut app, &wid);
}

/// A drain that times out leaves the workspace open; the Changes read it
/// cancelled must run again instead of leaving the list empty.
#[test]
fn failed_drain_reloads_the_changes_it_cancelled() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	let fx = two_repos();
	let tree_hold = fx.hold.with_file_name("tree-hold");
	let (mut app, wid) = spawn_two(&fx, Some(&tree_hold));

	// `b`'s tree read (no Git child) and status read both block.
	fs::write(&tree_hold, b"hold").unwrap();
	fs::write(&fx.hold, b"hold").unwrap();
	switch_repo(&app, &wid, "b", 1);
	lines_until(&app.rx, "[APP:TREE_IO_HELD]", Duration::from_secs(6));
	app.tracked.push(wait_git_child(app.pid, &app.starttime));

	click(&wid, "btn-workspace-menu");
	click(&wid, "btn-close-workspace");
	lines_until(
		&app.rx,
		"phase=draining intent=close-workspace",
		Duration::from_secs(4),
	);
	// Close cancels the status read; only the tree read keeps the drain.
	fs::remove_file(&fx.hold).unwrap();
	lines_until(
		&app.rx,
		"phase=failed intent=close-workspace reason=timeout",
		Duration::from_secs(14),
	);
	lines_until(&app.rx, "[APP:REPO_LOADED: b", Duration::from_secs(8));

	fs::remove_file(&tree_hold).unwrap();
	wait_git_idle(app.pid, &app.starttime);
	quit_cleanly(&mut app, &wid);
}

/// Losing the X server must end the process instead of spinning on the
/// dead connection's always-readable fd (vendor/gpui/SNIP_PATCH.md).
#[test]
fn x_server_loss_exits_instead_of_spinning() {
	let _lock = DisplayLock::acquire();
	if !require_display_tools() {
		return;
	}
	// A private server, so killing it leaves the shared DISPLAY alone.
	let mut xvfb = Command::new("Xvfb")
		.args(["-displayfd", "1", "-screen", "0", "1280x900x24"])
		.args(["-nolisten", "tcp"])
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("Xvfb should start");
	let mut display = String::new();
	BufReader::new(xvfb.stdout.take().unwrap())
		.read_line(&mut display)
		.expect("Xvfb display number");
	let display = format!(":{}", display.trim());
	let root = tempfile::tempdir().unwrap();
	let mut child = Command::new(native_bin())
		.args(["--workspace", &root.path().to_string_lossy()])
		.args(["--restore-dir", &root.path().to_string_lossy()])
		.env("DISPLAY", &display)
		.env("XMODIFIERS", "@im=none")
		.env("SNIP_NATIVE_E2E", "1")
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()
		.expect("native desktop should run");
	let pid = child.id();
	let (tx, rx) = std::sync::mpsc::channel::<String>();
	let stdout = child.stdout.take().unwrap();
	let reader = std::thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			let _ = tx.send(line);
		}
	});
	lines_until(&rx, "[APP:WINDOW_READY]", Duration::from_secs(20));
	std::thread::sleep(Duration::from_millis(500));

	let _ = xvfb.kill();
	let _ = xvfb.wait();
	// utime + stime in clock ticks.
	let ticks = |pid: u32| -> Option<u64> {
		let rest = proc_rest(pid)?;
		Some(rest[11].parse::<u64>().ok()? + rest[12].parse::<u64>().ok()?)
	};
	let before = ticks(pid).expect("app cpu ticks");
	let start = Instant::now();
	let mut last = before;
	let status = loop {
		if let Some(status) = child.try_wait().unwrap() {
			break status;
		}
		last = ticks(pid).unwrap_or(last);
		if start.elapsed() > Duration::from_secs(5) {
			let _ = child.kill();
			let _ = child.wait();
			panic!(
				"app still running 5s after its X server died; cpu ticks {before}->{last} (cleanup kill)"
			);
		}
		std::thread::sleep(Duration::from_millis(50));
	};
	let _ = reader.join();
	println!(
		"[LIFECYCLE-TEST] X loss: exited in {:?} with {status}, cpu ticks {before}->{last}",
		start.elapsed()
	);
	assert!(
		status.success(),
		"app exited uncleanly after X loss: {status}"
	);
}
