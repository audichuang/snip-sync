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

type Bounds = Arc<Mutex<std::collections::HashMap<String, [i32; 4]>>>;
type Viewport = Arc<Mutex<(i32, i32)>>;

struct DisplayLock(PathBuf);

impl DisplayLock {
	fn acquire() -> Self {
		let path =
			std::env::temp_dir().join("snip-native-lifecycle-display.lock");
		let start = Instant::now();
		loop {
			if fs::create_dir(&path).is_ok() {
				return Self(path);
			}
			if start.elapsed() > Duration::from_secs(120) {
				panic!("timed out waiting for the lifecycle display lock");
			}
			std::thread::sleep(Duration::from_millis(200));
		}
	}
}

impl Drop for DisplayLock {
	fn drop(&mut self) {
		let _ = fs::remove_dir(&self.0);
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
	for tool in ["xdotool", "xclip", "xwd", "convert", "git"] {
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
}

fn spawn_app(opts: SpawnOpts) -> App {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip-desktop-native"));
	cmd.args([
		"--workspace",
		&opts.workspace.to_string_lossy(),
		"--restore-dir",
		&opts.restore.to_string_lossy(),
	])
	.stdout(Stdio::piped())
	.stderr(Stdio::piped())
	.env("XMODIFIERS", "@im=none")
	.env("SNIP_NATIVE_E2E", "1")
	.env_remove("SNIP_E2E_GIT_HOLD_FILE")
	.env_remove("SNIP_NATIVE_E2E_READ_DELAY_MS")
	.env_remove("SNIP_NATIVE_E2E_APPLY_DELAY_MS");
	if let Some(ms) = opts.read_delay_ms {
		cmd.env("SNIP_NATIVE_E2E_READ_DELAY_MS", ms.to_string());
	}
	if let Some(ms) = opts.apply_delay_ms {
		cmd.env("SNIP_NATIVE_E2E_APPLY_DELAY_MS", ms.to_string());
	}
	if let Some(hold) = opts.hold_file {
		cmd.env("SNIP_E2E_GIT_HOLD_FILE", hold);
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
	let deadline = Instant::now() + timeout;
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
	while start.elapsed() < Duration::from_secs(8) {
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
	let deadline = Instant::now() + Duration::from_secs(4);
	loop {
		let found = BOUNDS.with(|slot| {
			slot.borrow()
				.as_ref()
				.and_then(|b| b.lock().unwrap().get(id).copied())
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
		let st = Command::new("magick")
			.arg(&xwd)
			.args(["-depth", "8", &format!("PNG24:{}", path.display())])
			.status()
			.expect("magick");
		assert!(st.success(), "magick failed for {}", path.display());
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
			None if start.elapsed() < timeout => {
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

fn shots() -> PathBuf {
	PathBuf::from("/tmp/snip-native-lifecycle-20260926-shots")
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
	});
	let wid = find_wid(app.pid);
	lines_until(&app.rx, "[APP:READY_REPOS: 1]", Duration::from_secs(12));
	let _ = Command::new("xdotool")
		.args(["windowsize", "--sync", &wid, "1080", "720"])
		.status();
	let _ = control("btn-workspace-menu");
	let _ = control("lifecycle-jobs");
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
	let deadline = Instant::now() + Duration::from_millis(800);
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
				None if start.elapsed() < Duration::from_secs(10) => {
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
	PathBuf::from(
		"/tmp/snip-native-lifecycle-export-integration-20260926-shots",
	)
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
	let deadline = Instant::now() + Duration::from_secs(8);
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
	let deadline = Instant::now() + Duration::from_secs(6);
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
	let deadline = Instant::now() + Duration::from_secs(6);
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
