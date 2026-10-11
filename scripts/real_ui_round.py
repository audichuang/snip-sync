#!/usr/bin/env python3
"""
scripts/real_ui_round.py - Setup, orchestration, and teardown for real-UI acceptance rounds.

Subcommands:
  prepare  - Set up isolated run directory, worktree, build .app, fixtures, snapshot config.
  launch   - Start desktop app with isolated environment for gate a or b.
  resize   - Resize the round's main window (title "snip-sync", or "<tab label> — snip-sync") to logical WxH via System Events.
  point    - Compute logical window and screen coordinates from CTRL_BOUNDS probe for action.json.
  title    - Print the exact title of the round's main window (the title oracle).
  finish   - Quit app via Cmd+Q, verify termination, restore clipboard, check config, clean up.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
from typing import Any, Dict, List, Optional, Tuple


REQUIRED_PROTOCOL_ANCESTOR = "f5247ab"
APP_WINDOW_TITLE = "snip-sync"
# The main window is titled "snip-sync" when no workspace tab is active, and
# "<active tab label> \u2014 snip-sync" (em dash) once a tab is open.
APP_WINDOW_TITLE_SEPARATOR = " \u2014 "


def is_app_window_title(name: Any) -> bool:
    """True for the main window title, with or without an active tab label."""
    if not isinstance(name, str):
        return False
    return name == APP_WINDOW_TITLE or name.endswith(APP_WINDOW_TITLE_SEPARATOR + APP_WINDOW_TITLE)


class MainWindowError(Exception):
    pass


def find_main_window(info: Dict[str, Any], pid: int) -> Dict[str, Any]:
    """The round's main window among the windows `pid` owns.

    The app also owns small helper windows (one is titled "Window"), so neither
    the first window nor any window will do: exactly one normal-layer window
    must carry the main title.
    """
    owned = [w for w in info.get("windows", []) if w.get("kCGWindowOwnerPID", pid) == pid]
    matching = [
        w for w in owned
        if w.get("kCGWindowLayer", 0) == 0 and is_app_window_title(w.get("kCGWindowName"))
    ]
    if len(matching) == 1:
        return matching[0]
    titles = [w.get("kCGWindowName") for w in owned]
    if not matching:
        hint = ""
        if owned and all(t is None for t in titles):
            hint = " No window has a name: grant Screen Recording to the terminal that runs this script."
        raise MainWindowError(
            f"No window titled '{APP_WINDOW_TITLE}' or '<label>{APP_WINDOW_TITLE_SEPARATOR}{APP_WINDOW_TITLE}' "
            f"for PID {pid}. Windows of this PID: {titles!r}.{hint}"
        )
    raise MainWindowError(f"{len(matching)} main windows for PID {pid}: {[w.get('kCGWindowName') for w in matching]!r}")


def applescript_string(text: str) -> str:
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


# The script that drives a round must be the one at the tested SHA: a round
# once ran an older checkout's copy, whose point/resize predated tab titles.
def script_sha256() -> str:
    return hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def check_round_script(env_data: Dict[str, Any]) -> Optional[str]:
    """An error when this script is not the copy `prepare` ran as."""
    want = env_data.get("round_script_sha256")
    if want == script_sha256():
        return None
    worktree = env_data.get("source_worktree")
    copy = f"{worktree}/scripts/real_ui_round.py" if worktree else "scripts/real_ui_round.py in a checkout at the tested SHA"
    return (
        f"{Path(__file__).resolve()} is not the script this round was prepared with "
        f"(prepared: {want or 'unknown'}). Use {copy}, the copy at the tested SHA {env_data.get('sha', '?')}."
    )


def load_round_env(run_dir: Path) -> Tuple[Optional[Dict[str, Any]], Optional[str]]:
    env_file = run_dir / "environment.json"
    if not env_file.is_file():
        return None, f"environment.json not found in {run_dir}"
    env_data = json.loads(env_file.read_text(encoding="utf-8"))
    return env_data, check_round_script(env_data)


# Bounds every ssh call except the remote build. stop_run_workers waits up to
# 10 s per worker, so this leaves room for several.
REMOTE_TIMEOUT_SECONDS = 120.0
REMOTE_BUILD_TIMEOUT_SECONDS = 3600.0


# Ensure ~/.cargo/bin is on PATH if present
cargo_bin_dir = str(Path.home() / ".cargo" / "bin")
if os.path.isdir(cargo_bin_dir) and cargo_bin_dir not in os.environ.get("PATH", ""):
    os.environ["PATH"] = f"{cargo_bin_dir}:{os.environ.get('PATH', '')}"


# -----------------------------------------------------------------------------
# System Operations Interface (for testability and platform independence)
# -----------------------------------------------------------------------------

@dataclasses.dataclass
class ProcessRecord:
    pid: int
    command: str


class SystemOperations:
    """Interface to OS-level operations, mockable in unit tests."""

    def move_mouse(self, x: float, y: float) -> None:
        """Moves the pointer to screen point (x, y), top-left origin, logical
        points. Computer-use tools have no hover-only action; this is how a
        tooltip is shown without clicking."""
        script = (
            'ObjC.import("CoreGraphics");'
            f"$.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent("
            f"null, $.kCGEventMouseMoved, {{x:{x},y:{y}}}, 0));"
        )
        subprocess.run(
            ["osascript", "-l", "JavaScript", "-e", script],
            check=True,
            capture_output=True,
            timeout=10,
        )

    def list_processes(self) -> List[ProcessRecord]:
        try:
            res = subprocess.run(
                ["ps", "-A", "-o", "pid=,command="],
                capture_output=True,
                text=True,
                check=True,
                timeout=5,
            )
            records = []
            for line in res.stdout.splitlines():
                line = line.strip()
                if not line:
                    continue
                parts = line.split(None, 1)
                if len(parts) == 2 and parts[0].isdigit():
                    records.append(ProcessRecord(pid=int(parts[0]), command=parts[1]))
            return records
        except Exception:
            return []

    def is_pid_alive(self, pid: int) -> bool:
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False

    def kill_process(self, pid: int, sig: signal.Signals = signal.SIGTERM) -> None:
        try:
            os.kill(pid, sig)
        except OSError:
            pass

    def wait_process_exit(self, pid: int, timeout_seconds: float = 10.0) -> Optional[int]:
        deadline = time.time() + timeout_seconds
        while time.time() < deadline:
            if not self.is_pid_alive(pid):
                return 0
            time.sleep(0.1)
        return None

    def run_applescript(self, script: str) -> str:
        res = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True,
            text=True,
            check=True,
            timeout=10,
        )
        return res.stdout.strip()

    def get_clipboard_bytes(self) -> bytes:
        if sys.platform == "darwin":
            res = subprocess.run(["pbpaste"], capture_output=True, check=True)
            return res.stdout
        return b""

    def set_clipboard_bytes(self, data: bytes) -> None:
        if sys.platform == "darwin":
            subprocess.run(["pbcopy"], input=data, check=True)

    def register_app(self, app_path: Path) -> None:
        if sys.platform == "darwin":
            lsregister_candidates = [
                "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
                "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister",
            ]
            for candidate in lsregister_candidates:
                if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                    subprocess.run(
                        [candidate, "-f", str(app_path)],
                        check=False,
                        timeout=10,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    return

    def unregister_app(self, app_path: Path) -> None:
        if sys.platform == "darwin":
            lsregister_candidates = [
                "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
                "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister",
            ]
            for candidate in lsregister_candidates:
                if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                    subprocess.run(
                        [candidate, "-u", str(app_path)],
                        check=False,
                        timeout=10,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    return

    def get_window_info(self, pid: int, helper_binary: Optional[Path] = None) -> Dict[str, Any]:
        """Query screen scale and window bounds for the given PID."""
        if helper_binary and helper_binary.is_file() and os.access(helper_binary, os.X_OK):
            out = subprocess.check_output([str(helper_binary), str(pid)], timeout=10)
            return json.loads(out)

        # Fallback to osascript / swift query if binary not present
        if sys.platform == "darwin":
            swift_script = f"""
import AppKit
import CoreGraphics
import Foundation
let pid = {pid}
let screens = NSScreen.screens.map {{ s -> [String: Any] in
    ["frame": [s.frame.origin.x, s.frame.origin.y, s.frame.width, s.frame.height], "scale": s.backingScaleFactor]
}}
let windows = (CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []).filter {{ ($0["kCGWindowOwnerPID"] as? Int) == pid }}
let data = try JSONSerialization.data(withJSONObject: ["screens": screens, "windows": windows], options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
"""
            out = subprocess.check_output(["swift", "-e", swift_script], timeout=10)
            return json.loads(out)

        return {"screens": [{"frame": [0, 0, 1920, 1080], "scale": 1.0}], "windows": []}

    def remote_exec(
        self, host: str, command: str, check: bool = True, timeout: Optional[float] = REMOTE_TIMEOUT_SECONDS
    ) -> subprocess.CompletedProcess:
        """A hung ssh comes back as exit 124 instead of stalling the round (raises when `check`)."""
        try:
            return subprocess.run(["ssh", host, command], capture_output=True, text=True, check=check, timeout=timeout)
        except subprocess.TimeoutExpired:
            if check:
                raise
            return subprocess.CompletedProcess(
                args=["ssh", host, command], returncode=124, stdout="", stderr=f"ssh timed out after {timeout} s"
            )


DEFAULT_SYS_OPS = SystemOperations()


# -----------------------------------------------------------------------------
# Configuration Directory & Snapshots
# -----------------------------------------------------------------------------

def get_real_config_dir() -> Path:
    """Return the platform default config directory as defined in crates/remote/src/lib.rs."""
    if "SNIP_CONFIG_DIR" in os.environ:
        return Path(os.environ["SNIP_CONFIG_DIR"])
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "com.audichuang.snip-sync"
    elif sys.platform == "win32":
        appdata = os.environ.get("APPDATA", str(Path.home() / "AppData" / "Roaming"))
        return Path(appdata) / "snip-sync"
    else:
        xdg = os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))
        return Path(xdg) / "snip-sync"


def snapshot_config_dir(config_dir: Path) -> Dict[str, Dict[str, Any]]:
    """Capture hashes and mtimes of all files in the config directory."""
    snapshot: Dict[str, Dict[str, Any]] = {}
    if not config_dir.exists():
        return snapshot
    for root, _, files in os.walk(config_dir):
        for f in files:
            p = Path(root) / f
            try:
                rel = str(p.relative_to(config_dir))
                st = p.stat()
                sha = hashlib.sha256(p.read_bytes()).hexdigest()
                snapshot[rel] = {
                    "size": st.st_size,
                    "mtime_ns": st.st_mtime_ns,
                    "sha256": sha,
                }
            except OSError:
                pass
    return snapshot


def verify_config_snapshot(config_dir: Path, expected: Dict[str, Dict[str, Any]]) -> List[str]:
    """Compare current directory against snapshot. Returns list of discrepancy descriptions."""
    current = snapshot_config_dir(config_dir)
    diffs: List[str] = []
    for rel, exp in expected.items():
        if rel not in current:
            diffs.append(f"Deleted file: {rel}")
        else:
            cur = current[rel]
            if cur["sha256"] != exp["sha256"]:
                diffs.append(f"Modified content: {rel} (sha256: {exp['sha256']} -> {cur['sha256']})")
            elif cur["mtime_ns"] != exp["mtime_ns"]:
                diffs.append(f"Modified mtime: {rel} (mtime: {exp['mtime_ns']} -> {cur['mtime_ns']})")
    for rel in current:
        if rel not in expected:
            diffs.append(f"Unexpected new file: {rel}")
    return diffs


# -----------------------------------------------------------------------------
# Leftover Detection & Cleaning
# -----------------------------------------------------------------------------

@dataclasses.dataclass
class LeftoverItem:
    kind: str  # "process", "export-hold", "remote-wrapper", "remote-worker"
    description: str
    provably_earlier_round: bool
    clean_target: Any = None


def is_snip_desktop_process(cmd: str) -> bool:
    """Check if the process executable is snip-desktop-native, handling paths with spaces."""
    cmd_clean = cmd.strip()
    if not cmd_clean:
        return False

    # Exclude common tools/interpreters that might pass snip-desktop-native in argument strings
    first_token = cmd_clean.split()[0]
    base_name = Path(first_token).name.lower()
    if base_name in ("agy", "node", "python", "python3", "grep", "git", "tail", "vim", "vi", "cargo"):
        return False

    # Match snip-desktop-native as an executable token (at start or after slash/space, followed by space or end)
    pattern = r"(?:^|[/\s])snip-desktop-native(?:\s|$)"
    return bool(re.search(pattern, cmd_clean))


def is_earlier_round_desktop_process(cmd: str) -> bool:
    """Return True if the process command line proves it belongs to a QA UI round."""
    markers = [
        "snip-sync-ui-runs",
        "snip-sync QA",
        "snip-ui-runs",
        "/Contents/MacOS/snip-desktop-native",
    ]
    return any(m in cmd for m in markers)


REMOTE_ROOT = "/home/audichuang/snip-ui-run"
REMOTE_WRAPPER = "~/.local/bin/snip"


def remote_run_dir(short_sha: str) -> str:
    return f"{REMOTE_ROOT}/{short_sha[:7]}"


def remote_backup_path(short_sha: str) -> str:
    """The wrapper's backup name while S11 of the remote protocol runs."""
    return f"{REMOTE_WRAPPER}.uirun-{short_sha[:7]}"


def is_round_wrapper(content: str) -> bool:
    return "snip-ui-run" in content or "SNIP_E2E_PASTE_HOLD" in content or "pids/$$" in content


# Section 2.3's stop_run_workers: kill only the PIDs this round's wrapper
# recorded whose arguments are exactly its worker command, and wait for each.
# A recorded PID with other arguments was reused after the worker exited.
STOP_RUN_WORKERS = r'''
failed=0
for record in "$W"/pids/*; do
  [ -f "$record" ] || continue
  pid=$(cat "$record")
  case "$pid" in
    ''|0*|*[!0-9]*) echo "invalid PID record: $record" >&2; failed=1; continue ;;
  esac
  args=$(ps -o args= -p "$pid" 2>/dev/null || true)
  [ -n "$args" ] || continue
  if [ "$args" != "$W/src/target/release/snip serve --stdio" ]; then
    # The worker exited and its PID now belongs to another process: a stale record.
    echo "PID $pid is no longer this round's worker, not killed: $args" >&2; continue
  fi
  kill "$pid" 2>/dev/null || true
  deadline=$((SECONDS + 10))
  while [ "$SECONDS" -lt "$deadline" ] && [ -n "$(ps -o args= -p "$pid" 2>/dev/null || true)" ]; do
    sleep 0.1
  done
  if [ -n "$(ps -o args= -p "$pid" 2>/dev/null || true)" ]; then
    echo "worker $pid did not exit within 10 s" >&2; failed=1
  else
    echo "stopped $pid"
  fi
done
exit "$failed"
'''


def stop_run_workers_command(w_remote: str) -> str:
    return f"W={shlex.quote(w_remote)} bash -c {shlex.quote(STOP_RUN_WORKERS)}"


def remote_sha256(host: str, path: str, sys_ops: SystemOperations) -> Optional[str]:
    """The file's sha256, "" when it is absent, None when ssh failed."""
    res = sys_ops.remote_exec(host, f"sha256sum {path} 2>/dev/null || true", check=False)
    if res.returncode != 0:
        return None
    out = res.stdout.split()
    return out[0] if out else ""


def clean_remote_round(host: str, sha: str, run_dir: Path, sys_ops: SystemOperations) -> Dict[str, Any]:
    """Section 6 of the remote protocol: this round's workers, wrapper, backup and $W, nothing else."""
    w_remote = remote_run_dir(sha)
    backup = remote_backup_path(sha)
    result: Dict[str, Any] = {"remote_dir": w_remote, "workers_stopped": False}

    records = sys_ops.remote_exec(host, f"cat '{w_remote}'/pids/* 2>/dev/null || true", check=False).stdout
    (run_dir / "workers-at-cleanup.txt").write_text(records, encoding="utf-8")
    stop = sys_ops.remote_exec(host, stop_run_workers_command(w_remote), check=False)
    result["stop_run_workers"] = {"exit": stop.returncode, "stdout": stop.stdout, "stderr": stop.stderr}
    if stop.returncode != 0:
        return result
    result["workers_stopped"] = True

    # S11 interrupted half way: put the wrapper back before checking it.
    sys_ops.remote_exec(
        host,
        f"if [ -e {backup} ] && [ ! -e {REMOTE_WRAPPER} ] && [ ! -L {REMOTE_WRAPPER} ]; then mv {backup} {REMOTE_WRAPPER}; fi",
        check=False,
    )
    installed_sha_file = run_dir / "snip-installed.sha"
    want = installed_sha_file.read_text(encoding="utf-8").split()[0] if installed_sha_file.is_file() else ""
    # A wrapper kept below that still execs into $W would turn into a broken
    # `snip` once $W is gone, so $W stays and the cleanup fails.
    blockers: List[str] = []
    for name, path in (("wrapper", REMOTE_WRAPPER), ("backup", backup)):
        got = remote_sha256(host, path, sys_ops)
        if got is None:
            result[name] = "unknown (ssh failed)"
            blockers.append(path)
        elif not got:
            result[name] = "absent"
        elif want and got == want:
            sys_ops.remote_exec(host, f"rm -f {path}", check=False)
            result[name] = "removed"
        else:
            result[name] = f"kept (sha256 {got})"
            content = sys_ops.remote_exec(host, f"cat {path} 2>/dev/null", check=False)
            if content.returncode != 0 or w_remote in content.stdout:
                print(f"ERROR: {host}:{path} still runs {w_remote} but its hash is not the recorded one.", file=sys.stderr)
                blockers.append(path)
            else:
                print(f"WARNING: {host}:{path} is not the wrapper this round installed; kept.", file=sys.stderr)
    if blockers:
        result["remote_dir_removed"] = False
        result["remote_dir_kept_for"] = blockers
        return result
    sys_ops.remote_exec(host, f"rm -rf '{w_remote}'", check=False)
    result["remote_dir_removed"] = True
    return result


def detect_leftovers(
    run_dir: Path,
    remote_host: Optional[str] = None,
    sys_ops: SystemOperations = DEFAULT_SYS_OPS,
    short_sha: Optional[str] = None,
) -> List[LeftoverItem]:
    """Identify leftover processes, files, and remote artifacts."""
    leftovers: List[LeftoverItem] = []

    # 1. Desktop processes
    for proc in sys_ops.list_processes():
        if is_snip_desktop_process(proc.command):
            provable = is_earlier_round_desktop_process(proc.command)
            leftovers.append(
                LeftoverItem(
                    kind="process",
                    description=f"PID {proc.pid}: {proc.command}",
                    provably_earlier_round=provable,
                    clean_target=proc.pid,
                )
            )

    # 2. Local export-hold
    hold_file = run_dir / "export-hold"
    if hold_file.exists():
        leftovers.append(
            LeftoverItem(
                kind="export-hold",
                description=f"Local export-hold file at {hold_file}",
                provably_earlier_round=True,
                clean_target=hold_file,
            )
        )

    # 3. Remote host checks
    if remote_host:
        # Check ~/.local/bin/snip
        check_cmd = "[ -e ~/.local/bin/snip ] || [ -L ~/.local/bin/snip ]"
        if sys_ops.remote_exec(remote_host, check_cmd, check=False).returncode == 0:
            content = sys_ops.remote_exec(
                remote_host, "cat ~/.local/bin/snip 2>/dev/null || true", check=False
            ).stdout
            provable = is_round_wrapper(content)
            leftovers.append(
                LeftoverItem(
                    kind="remote-wrapper",
                    description=f"Remote wrapper on {remote_host} at ~/.local/bin/snip",
                    provably_earlier_round=provable,
                    clean_target="~/.local/bin/snip",
                )
            )

        # This SHA's backup name only: other snip.uirun-* files belong to
        # other rounds or to the user, and are never touched.
        if short_sha:
            backup = remote_backup_path(short_sha)
            if sys_ops.remote_exec(remote_host, f"[ -e {backup} ] || [ -L {backup} ]", check=False).returncode == 0:
                content = sys_ops.remote_exec(remote_host, f"cat {backup} 2>/dev/null || true", check=False).stdout
                leftovers.append(
                    LeftoverItem(
                        kind="remote-wrapper",
                        description=f"Remote backup wrapper on {remote_host} at {backup}",
                        provably_earlier_round=is_round_wrapper(content),
                        clean_target=backup,
                    )
                )

    return leftovers


def clean_provable_leftovers(
    leftovers: List[LeftoverItem],
    remote_host: Optional[str] = None,
    sys_ops: SystemOperations = DEFAULT_SYS_OPS,
) -> List[LeftoverItem]:
    """Kill or remove only what is provably from an earlier round. Returns remaining leftovers."""
    remaining: List[LeftoverItem] = []
    for item in leftovers:
        if not item.provably_earlier_round:
            remaining.append(item)
            continue

        try:
            if item.kind == "process":
                pid = int(item.clean_target)
                sys_ops.kill_process(pid, signal.SIGTERM)
                if sys_ops.wait_process_exit(pid, timeout_seconds=3.0) is None:
                    sys_ops.kill_process(pid, signal.SIGKILL)
            elif item.kind == "export-hold":
                p = Path(item.clean_target)
                if p.exists():
                    p.unlink()
            elif item.kind == "remote-wrapper" and remote_host:
                target = str(item.clean_target)
                sys_ops.remote_exec(remote_host, f"rm -f {target}", check=False)
        except Exception:
            remaining.append(item)

    return remaining


# -----------------------------------------------------------------------------
# Log Parsing & Coordinate Math
# -----------------------------------------------------------------------------

RE_CTRL_BOUNDS = re.compile(
    r"^\[APP:CTRL_BOUNDS:\s+id=(.*?)\s+x=([-\d.]+)\s+y=([-\d.]+)\s+w=([-\d.]+)\s+h=([-\d.]+)\]$"
)
RE_CTRL_GONE = re.compile(r"^\[APP:CTRL_GONE:\s+id=(.*?)\]$")
RE_VIEWPORT = re.compile(r"^\[APP:VIEWPORT:\s+(\d+)x(\d+)\]$")


@dataclasses.dataclass
class ControlBoundsMatch:
    control_id: str
    x: float
    y: float
    w: float
    h: float
    line_number: int
    raw_text: str


def parse_control_bounds_from_lines(lines: List[str], target_id: str) -> ControlBoundsMatch:
    """Find the latest active CTRL_BOUNDS for target_id not followed by CTRL_GONE."""
    latest: Optional[ControlBoundsMatch] = None
    for n, line in enumerate(lines, 1):
        line = line.strip()
        m_bounds = RE_CTRL_BOUNDS.match(line)
        if m_bounds and m_bounds.group(1) == target_id:
            latest = ControlBoundsMatch(
                control_id=target_id,
                x=float(m_bounds.group(2)),
                y=float(m_bounds.group(3)),
                w=float(m_bounds.group(4)),
                h=float(m_bounds.group(5)),
                line_number=n,
                raw_text=line,
            )
        elif RE_CTRL_GONE.match(line):
            m_gone = RE_CTRL_GONE.match(line)
            if m_gone and m_gone.group(1) == target_id:
                latest = None

    if latest is None:
        raise ValueError(f"missing-control {target_id}")
    if latest.w < 1.0 or latest.h < 1.0:
        raise ValueError(f"invalid-control-dimensions {target_id} w={latest.w} h={latest.h}")
    return latest


def parse_latest_viewport(lines: List[str]) -> Tuple[int, int]:
    """Return the latest (Wp, Hp) physical pixels from [APP:VIEWPORT: ...]."""
    for line in reversed(lines):
        line = line.strip()
        m = RE_VIEWPORT.match(line)
        if m:
            return int(m.group(1)), int(m.group(2))
    raise ValueError("No [APP:VIEWPORT: ...] line found in log")


def calculate_point_coordinates(
    bounds: ControlBoundsMatch,
    viewport_physical: Tuple[int, int],
    window_frame: Dict[str, float],
    scale: float,
) -> Dict[str, Any]:
    """Calculate logical window and screen coordinates according to protocol section 3."""
    frame_x = window_frame["X"]
    frame_y = window_frame["Y"]
    frame_w = window_frame["Width"]
    frame_h = window_frame["Height"]

    wp, hp = viewport_physical

    # Scale check: Wp should equal frame_w * scale
    expected_wp = round(frame_w * scale)
    if wp != expected_wp:
        raise ValueError(
            f"Viewport physical width {wp} does not match frame logical width {frame_w} * scale {scale} = {expected_wp}"
        )

    titlebar_h = frame_h - (hp / scale)
    content_origin = [frame_x, frame_y + titlebar_h]

    # Content logical center relative to content top-left
    cx_content = (bounds.x + bounds.w / 2.0) / scale
    cy_content = (bounds.y + bounds.h / 2.0) / scale

    # Tool point (window-relative including titlebar)
    tool_point = [cx_content, cy_content + titlebar_h]

    # Screen point
    screen_point = [content_origin[0] + cx_content, content_origin[1] + cy_content]

    return {
        "id": bounds.control_id,
        "bounds": [bounds.x, bounds.y, bounds.w, bounds.h],
        "bounds_line": bounds.line_number,
        "bounds_text": bounds.raw_text,
        "viewport": [wp, hp],
        "scale": scale,
        "titlebar": titlebar_h,
        "content_origin": content_origin,
        "tool_point": [round(tool_point[0], 2), round(tool_point[1], 2)],
        "screen_point": [round(screen_point[0], 2), round(screen_point[1], 2)],
        "kind": "click",
        "tool_coordinate_system": "window-relative",
    }


# -----------------------------------------------------------------------------
# Environment Schema & Validation
# -----------------------------------------------------------------------------

ENVIRONMENT_SCHEMA_REQUIRED_KEYS = [
    "sha",
    "branch",
    "source_worktree",
    "build",
    "binary",
    "binary_sha256",
    "app_bundle",
    "config_isolated",
    "export_hold_file",
    "paste_id",
    "fixtures",
    "fixture_logs",
    "launch_env",
    "launch_gate_b",
    "launch_gate_a",
    "prepared_by",
    "viewport_measured",
]


def validate_environment_schema(data: Dict[str, Any]) -> List[str]:
    """Ensure environment.json adheres to the mandatory specification."""
    errors = []
    for key in ENVIRONMENT_SCHEMA_REQUIRED_KEYS:
        if key not in data:
            errors.append(f"Missing required key '{key}'")

    if "fixtures" in data:
        if not isinstance(data["fixtures"], dict):
            errors.append("'fixtures' must be a dictionary")
        else:
            for fix_key in ["gate_a", "gate_b", "perf15"]:
                if fix_key not in data["fixtures"]:
                    errors.append(f"Missing fixture path '{fix_key}' in 'fixtures'")

    if "launch_env" in data:
        if not isinstance(data["launch_env"], dict):
            errors.append("'launch_env' must be a dictionary")
        else:
            for env_key in [
                "SNIP_NATIVE_E2E",
                "SNIP_THEME",
                "SNIP_CONFIG_DIR",
                "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE",
            ]:
                if env_key not in data["launch_env"]:
                    errors.append(f"Missing environment variable '{env_key}' in 'launch_env'")

    return errors


# -----------------------------------------------------------------------------
# Helper Utilities
# -----------------------------------------------------------------------------

def resolve_run_dir(given: Optional[str]) -> Path:
    if given:
        return Path(given).resolve()
    if "SNIP_RUN_DIR" in os.environ:
        return Path(os.environ["SNIP_RUN_DIR"]).resolve()
    cwd_env = Path.cwd() / "environment.json"
    if cwd_env.is_file():
        return Path.cwd().resolve()
    # Default to home directory standard
    today = datetime.date.today().isoformat()
    return (Path.home() / "snip-sync-ui-runs" / f"{today}-round").resolve()


def sha256_of_file(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        while chunk := f.read(65536):
            h.update(chunk)
    return h.hexdigest()


def detect_paste_id_format(repo_or_worktree: Path) -> str:
    paste_rs = repo_or_worktree / "crates" / "desktop-native" / "src" / "paste.rs"
    if paste_rs.is_file():
        content = paste_rs.read_text(encoding="utf-8")
        if "paste-<kind>:<ix>:<path>" in content or "paste-row:3:" in content:
            return "paste-<row|include|overwrite>:<ix>:<path>"
    return "paste-row:<path>"


# -----------------------------------------------------------------------------
# Subcommands
# -----------------------------------------------------------------------------

def cmd_prepare(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    rev = args.sha
    clean_leftovers = args.clean_leftovers
    remote_host = args.remote
    allow_running_desktop = getattr(args, "allow_running_desktop", False)

    # 1. Validate SHA ancestor requirement
    chk = subprocess.run(
        ["git", "merge-base", "--is-ancestor", REQUIRED_PROTOCOL_ANCESTOR, rev],
        capture_output=True,
    )
    if chk.returncode != 0:
        print(
            f"ERROR: Revision '{rev}' does not contain required protocol ancestor {REQUIRED_PROTOCOL_ANCESTOR}.",
            file=sys.stderr,
        )
        return 1

    full_sha = subprocess.check_output(["git", "rev-parse", rev], text=True).strip()
    short_sha = full_sha[:7]

    tested_script = subprocess.run(
        ["git", "show", f"{full_sha}:scripts/real_ui_round.py"], capture_output=True
    )
    if tested_script.returncode != 0 or hashlib.sha256(tested_script.stdout).hexdigest() != script_sha256():
        print(
            f"ERROR: {Path(__file__).resolve()} differs from scripts/real_ui_round.py at {short_sha}. "
            "Run the copy in a checkout at the tested SHA.",
            file=sys.stderr,
        )
        return 1

    # Branch name
    try:
        branch = subprocess.check_output(
            ["git", "rev-parse", "--abbrev-ref", rev], text=True
        ).strip()
        if not branch or branch == "HEAD" or branch == rev:
            branch = "(detached)"
    except Exception:
        branch = "(detached)"

    # Resolve run directory
    today = datetime.date.today().isoformat()
    default_name = f"{today}-{short_sha}-remote" if remote_host else f"{today}-{short_sha}"
    run_dir = Path(args.run).resolve() if args.run else (Path.home() / "snip-sync-ui-runs" / default_name)

    print(f"=== Preparing Real-UI Acceptance Round ===")
    print(f"SHA:     {full_sha} ({short_sha})")
    print(f"Run dir: {run_dir}")
    if remote_host:
        print(f"Remote:  {remote_host}")

    # 2. Check for leftovers
    leftovers = detect_leftovers(run_dir, remote_host=remote_host, sys_ops=sys_ops, short_sha=short_sha)
    if leftovers:
        if clean_leftovers:
            print("Cleaning provable earlier round leftovers...")
            leftovers = clean_provable_leftovers(leftovers, remote_host=remote_host, sys_ops=sys_ops)

        if leftovers and not allow_running_desktop:
            print("\nERROR: Refusing to start because leftovers were detected:", file=sys.stderr)
            for item in leftovers:
                earlier_str = "provably earlier round" if item.provably_earlier_round else "NOT proven earlier round"
                print(f"  - [{item.kind}] {item.description} ({earlier_str})", file=sys.stderr)
            print("\nUse --clean-leftovers to remove provable leftovers.", file=sys.stderr)
            return 1

    run_dir.mkdir(parents=True, exist_ok=True)

    # 3. Create detached worktree
    worktree_dir = run_dir / "worktree"
    if not worktree_dir.exists():
        print(f"Creating detached worktree at {worktree_dir}...")
        subprocess.run(["git", "worktree", "add", "--detach", str(worktree_dir), full_sha], check=True)

    # Copy target if provided
    if getattr(args, "copy_target_from", None):
        src_target = Path(args.copy_target_from)
        dest_target = worktree_dir / "target"
        if src_target.is_dir() and not dest_target.exists():
            print(f"Copying cargo target directory from {src_target}...")
            subprocess.run(["cp", "-c", "-R", str(src_target), str(dest_target)], check=True)

    # 4. Cargo build
    print("Building snip-desktop-native (--locked)...")
    subprocess.run(
        ["cargo", "build", "-p", "snip-desktop-native", "--locked"],
        cwd=worktree_dir,
        check=True,
    )
    built_bin = worktree_dir / "target" / "debug" / "snip-desktop-native"
    if not built_bin.is_file():
        print(f"ERROR: Expected binary not found at {built_bin}", file=sys.stderr)
        return 1
    bin_sha = sha256_of_file(built_bin)

    # Isolated config directory
    isolated_config = run_dir / "config"
    isolated_config.mkdir(parents=True, exist_ok=True)

    # 5. Bundle into uniquely named .app
    app_name = f"snip-sync QA {short_sha}.app"
    app_dir = run_dir / app_name
    macos_dir = app_dir / "Contents" / "MacOS"
    macos_dir.mkdir(parents=True, exist_ok=True)

    dest_bin = macos_dir / "snip-desktop-native"
    shutil.copy2(built_bin, dest_bin)
    dest_bin.chmod(0o755)

    if sha256_of_file(dest_bin) != bin_sha:
        print("ERROR: Binary SHA-256 verification failed after bundling!", file=sys.stderr)
        return 1

    launcher_script = macos_dir / "snip-desktop-launcher"
    launcher_content = f"""#!/bin/sh
# Ensures environment isolation and logging when opened via LaunchServices (e.g. open -a or getApp).
export SNIP_NATIVE_E2E="${{SNIP_NATIVE_E2E:-1}}"
export SNIP_THEME="${{SNIP_THEME:-dark}}"
export SNIP_CONFIG_DIR="${{SNIP_CONFIG_DIR:-{isolated_config}}}"
export SNIP_NATIVE_E2E_EXPORT_HOLD_FILE="${{SNIP_NATIVE_E2E_EXPORT_HOLD_FILE:-{run_dir / "export-hold"}}}"

if [ -n "$SNIP_APP_LOG" ]; then
    LOG_FILE="$SNIP_APP_LOG"
elif [ -f "{run_dir}/app-gate-a.log" ] && [ ! -f "{run_dir}/app-gate-b.log" ]; then
    LOG_FILE="{run_dir}/app-gate-a.log"
else
    LOG_FILE="{run_dir}/app-gate-b.log"
fi

exec "{dest_bin}" "$@" >> "$LOG_FILE" 2>&1
"""
    launcher_script.write_text(launcher_content, encoding="utf-8")
    launcher_script.chmod(0o755)

    bundle_id = f"com.snipsync.qa.{run_dir.name}"
    info_plist_content = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key>
	<string>snip-sync QA {short_sha}</string>
	<key>CFBundleExecutable</key>
	<string>snip-desktop-launcher</string>
	<key>CFBundleIdentifier</key>
	<string>{bundle_id}</string>
	<key>CFBundleName</key>
	<string>snip-sync QA {short_sha}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSEnvironment</key>
	<dict>
		<key>SNIP_NATIVE_E2E</key>
		<string>1</string>
		<key>SNIP_THEME</key>
		<string>dark</string>
		<key>SNIP_CONFIG_DIR</key>
		<string>{isolated_config}</string>
		<key>SNIP_NATIVE_E2E_EXPORT_HOLD_FILE</key>
		<string>{run_dir / "export-hold"}</string>
	</dict>
</dict>
</plist>
"""
    (app_dir / "Contents" / "Info.plist").write_text(info_plist_content, encoding="utf-8")

    # Register fresh bundle with LaunchServices
    sys_ops.register_app(app_dir)
    print("[OK] Registered .app with LaunchServices.")

    # Compile window-info helper if swiftc available
    window_swift_src = run_dir / "window.swift"
    window_info_bin = run_dir / "window-info"
    window_swift_src.write_text("""import AppKit
import CoreGraphics
import Foundation
let pid = Int(CommandLine.arguments[1])!
let screens = NSScreen.screens.map { s -> [String: Any] in
    ["frame": [s.frame.origin.x, s.frame.origin.y, s.frame.width, s.frame.height], "scale": s.backingScaleFactor]
}
let windows = (CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []).filter { ($0["kCGWindowOwnerPID"] as? Int) == pid }
let pointer = NSEvent.mouseLocation
let data = try JSONSerialization.data(withJSONObject: ["screens": screens, "windows": windows, "pointer_appkit": [pointer.x, pointer.y]], options: [.prettyPrinted, .sortedKeys])
print(String(data: data, encoding: .utf8)!)
""", encoding="utf-8")

    if sys.platform == "darwin" and shutil.which("swiftc"):
        try:
            subprocess.run(["swiftc", "-O", str(window_swift_src), "-o", str(window_info_bin)], check=False, timeout=15)
        except Exception:
            pass

    # 6. Generate fixtures
    print("Generating Gate B fixtures...")
    log_gate_b = run_dir / "fixture-gate-b.log"
    with log_gate_b.open("w", encoding="utf-8") as f:
        subprocess.run(["sh", "docs/real-ui-operator-fixture.sh", str(run_dir / "gate-b")], stdout=f, stderr=subprocess.STDOUT, check=True)

    print("Generating perf15 fixture...")
    log_perf15 = run_dir / "fixture-perf15.log"
    with log_perf15.open("w", encoding="utf-8") as f:
        subprocess.run(
            ["python3", "scripts/workload_generator.py", str(run_dir / "perf15"), "--repos", "15", "--files", "1000", "--commits", "1000", "--refs", "30", "--quiet"],
            stdout=f,
            stderr=subprocess.STDOUT,
            check=True,
        )

    print("Generating Gate A fixtures (+ verify)...")
    log_gate_a = run_dir / "fixture-gate-a.log"
    with log_gate_a.open("w", encoding="utf-8") as f:
        subprocess.run(["python3", "scripts/collaboration_fixture.py", "generate", "--output", str(run_dir / "gate-a")], stdout=f, stderr=subprocess.STDOUT, check=True)
        v = subprocess.run(["python3", "scripts/collaboration_fixture.py", "verify", "--fixture", str(run_dir / "gate-a")], stdout=f, stderr=subprocess.STDOUT, check=True)
        if v.returncode != 0:
            print("ERROR: Gate A verification failed!", file=sys.stderr)
            return 1

    # 7. Snapshot real config
    real_config_dir = get_real_config_dir()
    real_snap = snapshot_config_dir(real_config_dir)
    (run_dir / "real-config-snapshot.json").write_text(json.dumps(real_snap, indent=2), encoding="utf-8")

    # 8. Save clipboard bytes
    clip_bytes = sys_ops.get_clipboard_bytes()
    clip_file = run_dir / "clipboard-initial.bin"
    clip_file.write_bytes(clip_bytes)
    clip_sha = hashlib.sha256(clip_bytes).hexdigest()

    # 9. Handle --remote setup if requested
    if remote_host:
        print(f"Setting up remote fixtures on {remote_host}...")
        w_remote = remote_run_dir(short_sha)
        # Build snip-cli for cross checks
        subprocess.run(["cargo", "build", "-p", "snip-cli", "--locked"], cwd=worktree_dir, check=True)
        # Setup local ws
        local_ws = run_dir / "local-ws"
        local_ws.mkdir(parents=True, exist_ok=True)
        g_cmd = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run(g_cmd + ["init", "-q", str(local_ws)], check=True)
        (local_ws / "local.txt").write_text("local\n", encoding="utf-8")
        subprocess.run(g_cmd + ["-C", str(local_ws), "add", "."], check=True)
        subprocess.run(g_cmd + ["-C", str(local_ws), "commit", "-qm", "init"], check=True)

        # Build and install wrapper on remote
        sys_ops.remote_exec(remote_host, f"rm -rf '{w_remote}' && mkdir -p '{w_remote}/src' '{w_remote}/pids'")
        archive_proc = subprocess.Popen(["git", "archive", full_sha], stdout=subprocess.PIPE)
        subprocess.run(["ssh", remote_host, f"tar -x -C '{w_remote}/src'"], stdin=archive_proc.stdout, check=True)
        archive_proc.wait()

        sys_ops.remote_exec(
            remote_host,
            f"cd '{w_remote}/src' && export PATH=$HOME/.cargo/bin:$PATH && cargo build --release -p snip-cli --locked",
            check=True,
            timeout=REMOTE_BUILD_TIMEOUT_SECONDS,
        )
        wrapper_sh = f"""#!/bin/sh
echo $$ > "{w_remote}/pids/$$"
export SNIP_E2E_PASTE_HOLD="{w_remote}/paste-hold"
exec "{w_remote}/src/target/release/snip" "$@"
"""
        sys_ops.remote_exec(remote_host, "mkdir -p ~/.local/bin")
        p = subprocess.Popen(["ssh", remote_host, "cat > ~/.local/bin/snip && chmod +x ~/.local/bin/snip"], stdin=subprocess.PIPE, text=True)
        p.communicate(input=wrapper_sh)

        remote_sha_out = sys_ops.remote_exec(remote_host, "sha256sum ~/.local/bin/snip").stdout.strip()
        (run_dir / "snip-installed.sha").write_text(remote_sha_out + "\n", encoding="utf-8")

    # 10. Write environment.json
    paste_id = detect_paste_id_format(worktree_dir)
    gate_b_ws = run_dir / "gate-b" / "fixtures" / "ws-src"
    gate_a_ws = run_dir / "gate-a" / "machine-a"

    env_data = {
        "sha": full_sha,
        "branch": branch,
        "source_worktree": str(worktree_dir),
        "build": "cargo build -p snip-desktop-native --locked (debug)",
        "binary": str(dest_bin),
        "binary_sha256": bin_sha,
        "app_bundle": str(app_dir),
        "config_isolated": str(isolated_config),
        "export_hold_file": f"{run_dir}/export-hold (absent at start)",
        "paste_id": paste_id,
        "fixtures": {
            "gate_b": str(run_dir / "gate-b" / "fixtures"),
            "perf15": str(run_dir / "perf15"),
            "gate_a": str(run_dir / "gate-a"),
        },
        "fixture_logs": [
            "fixture-gate-b.log",
            "fixture-perf15.log",
            "fixture-gate-a.log (verify exit 0)",
        ],
        "launch_env": {
            "SNIP_NATIVE_E2E": "1",
            "SNIP_THEME": "dark",
            "SNIP_CONFIG_DIR": str(isolated_config),
            "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE": str(run_dir / "export-hold"),
        },
        "launch_gate_b": f'"{dest_bin}" --workspace "{gate_b_ws}" > "{run_dir}/app-gate-b.log" 2>&1',
        "launch_gate_a": f'"{dest_bin}" --workspace "{gate_a_ws}" > "{run_dir}/app-gate-a.log" 2>&1',
        "prepared_by": f"scripts/real_ui_round.py, {datetime.date.today().isoformat()}",
        "viewport_measured": "(to be filled by the operator)",
        "clipboard_snapshot_sha256": clip_sha,
        "remote_host": remote_host,
        "round_script_sha256": script_sha256(),
    }

    errs = validate_environment_schema(env_data)
    if errs:
        print(f"ERROR: environment.json schema validation failed: {errs}", file=sys.stderr)
        return 1

    (run_dir / "environment.json").write_text(json.dumps(env_data, indent=2), encoding="utf-8")
    print(f"\n[OK] Round prepared successfully in {run_dir}")
    print(f"  App bundle: {app_dir}")
    print(f"  Binary SHA: {bin_sha}")
    return 0


def cmd_supervisor(args: argparse.Namespace) -> int:
    gate = args.gate.lower()
    run_dir = resolve_run_dir(args.run)
    env_file = run_dir / "environment.json"
    if not env_file.is_file():
        print(f"ERROR: environment.json not found in {run_dir}", file=sys.stderr)
        return 1
    script_err = check_round_script(json.loads(env_file.read_text(encoding="utf-8")))
    if script_err:
        print(f"ERROR: {script_err}", file=sys.stderr)
        return 1

    env_data = json.loads(env_file.read_text(encoding="utf-8"))
    binary = Path(env_data["binary"])
    if not binary.is_file():
        print(f"ERROR: Binary not found at {binary}", file=sys.stderr)
        return 1

    workspace = (
        Path(env_data["fixtures"]["gate_a"]) / "machine-a"
        if gate == "a"
        else Path(env_data["fixtures"]["gate_b"]) / "ws-src"
    )

    log_file = run_dir / f"app-gate-{gate}.log"
    proc_env = os.environ.copy()
    proc_env.update(env_data["launch_env"])
    proc_env["SNIP_APP_LOG"] = str(log_file)

    started_at = datetime.datetime.now().astimezone().isoformat()
    log_fp = open(log_file, "a", encoding="utf-8")

    app_proc = subprocess.Popen(
        [str(binary), "--workspace", str(workspace)],
        env=proc_env,
        stdout=log_fp,
        stderr=subprocess.STDOUT,
    )

    proc_state = {
        "pid": app_proc.pid,
        "supervisor_pid": os.getpid(),
        "gate": gate,
        "log": str(log_file),
        "launch_time": time.time(),
        "started_at": started_at,
    }
    (run_dir / "app-process.json").write_text(json.dumps(proc_state, indent=2), encoding="utf-8")

    ret = app_proc.wait()
    finished_at = datetime.datetime.now().astimezone().isoformat()
    try:
        log_fp.flush()
        log_fp.close()
    except Exception:
        pass

    sig = -ret if ret < 0 else None
    exit_data = {
        "pid": app_proc.pid,
        "exit_code": ret,
        "signal": sig,
        "started_at": started_at,
        "finished_at": finished_at,
    }
    exit_file = run_dir / f"app-gate-{gate}-exit.json"
    exit_file.write_text(json.dumps(exit_data, indent=2), encoding="utf-8")
    return 0


def cmd_launch(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    gate = args.gate.lower()
    if gate not in ("a", "b"):
        print("ERROR: --gate must be 'a' or 'b'", file=sys.stderr)
        return 1

    run_dir = resolve_run_dir(args.run)
    env_file = run_dir / "environment.json"
    if not env_file.is_file():
        print(f"ERROR: environment.json not found in {run_dir}", file=sys.stderr)
        return 1
    script_err = check_round_script(json.loads(env_file.read_text(encoding="utf-8")))
    if script_err:
        print(f"ERROR: {script_err}", file=sys.stderr)
        return 1

    env_data = json.loads(env_file.read_text(encoding="utf-8"))
    binary = Path(env_data["binary"])
    if not binary.is_file():
        print(f"ERROR: Binary not found at {binary}", file=sys.stderr)
        return 1

    workspace = (
        Path(env_data["fixtures"]["gate_a"]) / "machine-a"
        if gate == "a"
        else Path(env_data["fixtures"]["gate_b"]) / "ws-src"
    )

    log_file = run_dir / f"app-gate-{gate}.log"
    exit_file = run_dir / f"app-gate-{gate}-exit.json"
    proc_file = run_dir / "app-process.json"

    # Remove any stale exit or process file from previous attempts
    if exit_file.exists():
        exit_file.unlink()
    if proc_file.exists():
        proc_file.unlink()

    print(f"Launching snip-desktop-native supervisor for Gate {gate.upper()}...")
    print(f"  Binary:    {binary}")
    print(f"  Workspace: {workspace}")
    print(f"  Log file:  {log_file}")

    supervisor_cmd = [
        sys.executable,
        str(Path(__file__).resolve()),
        "_supervisor",
        "--gate", gate,
        "--run", str(run_dir),
    ]

    kwargs: Dict[str, Any] = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
    }
    if sys.platform != "win32":
        kwargs["start_new_session"] = True

    subprocess.Popen(supervisor_cmd, **kwargs)

    # Bounded wait for supervisor to write app-process.json and ensure PID is alive
    deadline = time.time() + 5.0
    app_pid = None
    proc_state = None
    while time.time() < deadline:
        if proc_file.is_file():
            try:
                data = json.loads(proc_file.read_text(encoding="utf-8"))
                if data.get("gate") == gate and data.get("pid"):
                    app_pid = data["pid"]
                    if sys_ops.is_pid_alive(app_pid):
                        proc_state = data
                        break
            except Exception:
                pass
        time.sleep(0.1)

    if not app_pid or not proc_state:
        if exit_file.is_file():
            print(f"ERROR: App exited immediately: {exit_file.read_text(encoding='utf-8')}", file=sys.stderr)
        else:
            print("ERROR: Timed out waiting for app process to start.", file=sys.stderr)
        return 1

    print(f"[OK] Started PID {app_pid} (supervisor PID {proc_state.get('supervisor_pid')}). Recorded to app-process.json.")
    return 0


def cmd_resize(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    width = int(args.width)
    height = int(args.height)
    run_dir = resolve_run_dir(args.run)
    _, env_err = load_round_env(run_dir)
    if env_err:
        print(f"ERROR: {env_err}", file=sys.stderr)
        return 1

    proc_file = run_dir / "app-process.json"
    if not proc_file.is_file():
        print(f"ERROR: app-process.json not found in {run_dir}. Launch app first.", file=sys.stderr)
        return 1

    proc_state = json.loads(proc_file.read_text(encoding="utf-8"))
    pid = proc_state["pid"]
    log_file = Path(proc_state["log"])

    if not sys_ops.is_pid_alive(pid):
        print(f"ERROR: Process PID {pid} is not running.", file=sys.stderr)
        return 1

    # Query window info
    helper_bin = run_dir / "window-info"
    info = sys_ops.get_window_info(pid, helper_binary=helper_bin)
    try:
        win = find_main_window(info, pid)
    except MainWindowError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    frame = win.get("kCGWindowBounds", {})
    scale = info.get("screens", [{}])[0].get("scale", 1.0)

    # Determine titlebar height from previous viewport if available, else standard 32
    titlebar = 32.0
    if log_file.is_file():
        try:
            lines = log_file.read_text(encoding="utf-8", errors="replace").splitlines()
            prev_wp, prev_hp = parse_latest_viewport(lines)
            titlebar = frame.get("Height", 632) - (prev_hp / scale)
        except Exception:
            titlebar = 32.0

    target_win_w = width
    target_win_h = height + int(round(titlebar))

    title = win["kCGWindowName"]
    print(f"Resizing '{title}' (PID {pid}) to logical content {width}x{height} (window: {target_win_w}x{target_win_h})...")
    script = f'tell application "System Events" to tell first application process whose unix id is {pid}\nset size of window {applescript_string(title)} to {{{target_win_w}, {target_win_h}}}\nend tell'
    sys_ops.run_applescript(script)

    # Bounded wait for latest VIEWPORT matching expected physical size
    expected_wp = round(width * scale)
    expected_hp = round(height * scale)
    timeout = 10.0
    deadline = time.time() + timeout

    matched = False
    latest_vp = None
    while time.time() < deadline:
        if log_file.is_file():
            lines = log_file.read_text(encoding="utf-8", errors="replace").splitlines()
            try:
                latest_vp = parse_latest_viewport(lines)
                if latest_vp == (expected_wp, expected_hp):
                    matched = True
                    break
            except Exception:
                pass
        time.sleep(0.1)

    if not matched:
        print(
            f"ERROR: Timed out waiting for [APP:VIEWPORT: {expected_wp}x{expected_hp}]. Latest: {latest_vp}",
            file=sys.stderr,
        )
        return 1

    print(f"[OK] Viewport confirmed at {expected_wp}x{expected_hp} (scale: {scale}).")
    return 0


def cmd_point(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    control_id = args.control_id
    run_dir = resolve_run_dir(args.run)
    _, env_err = load_round_env(run_dir)
    if env_err:
        print(f"ERROR: {env_err}", file=sys.stderr)
        return 1

    proc_file = run_dir / "app-process.json"
    if not proc_file.is_file():
        print(f"ERROR: app-process.json not found in {run_dir}", file=sys.stderr)
        return 1

    proc_state = json.loads(proc_file.read_text(encoding="utf-8"))
    pid = proc_state["pid"]
    log_file = Path(proc_state["log"])

    if not log_file.is_file():
        print(f"ERROR: Log file not found at {log_file}", file=sys.stderr)
        return 1

    if not sys_ops.is_pid_alive(pid):
        print(f"ERROR: Process PID {pid} is not running.", file=sys.stderr)
        return 1

    lines = log_file.read_text(encoding="utf-8", errors="replace").splitlines()
    bounds = parse_control_bounds_from_lines(lines, control_id)
    viewport = parse_latest_viewport(lines)

    helper_bin = run_dir / "window-info"
    info = sys_ops.get_window_info(pid, helper_binary=helper_bin)
    try:
        win = find_main_window(info, pid)
    except MainWindowError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    frame = win["kCGWindowBounds"]
    scale = info.get("screens", [{}])[0].get("scale", 1.0)

    res = calculate_point_coordinates(bounds, viewport, frame, scale)

    print(f"Control:               {control_id}")
    print(f"Logical Window Point:  {res['tool_point']}")
    print(f"Screen Point:          {res['screen_point']}")
    if getattr(args, "hover", None):
        x, y = res["screen_point"]
        sys_ops.move_mouse(x, y)
        time.sleep(args.hover)
        res["hover_seconds"] = args.hover
        print(f"Hovered at {res['screen_point']} for {args.hover}s (no click)")
    print("\nDraft action.json entry:")
    print(json.dumps(res, indent=2, ensure_ascii=False))
    return 0


def cmd_title(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    run_dir = resolve_run_dir(args.run)
    _, env_err = load_round_env(run_dir)
    if env_err:
        print(f"ERROR: {env_err}", file=sys.stderr)
        return 1
    proc_file = run_dir / "app-process.json"
    if not proc_file.is_file():
        print(f"ERROR: app-process.json not found in {run_dir}. Launch app first.", file=sys.stderr)
        return 1
    pid = json.loads(proc_file.read_text(encoding="utf-8"))["pid"]
    if not sys_ops.is_pid_alive(pid):
        print(f"ERROR: Process PID {pid} is not running.", file=sys.stderr)
        return 1
    info = sys_ops.get_window_info(pid, helper_binary=run_dir / "window-info")
    try:
        win = find_main_window(info, pid)
    except MainWindowError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    print(win["kCGWindowName"])
    return 0


def cmd_finish(args: argparse.Namespace, sys_ops: SystemOperations = DEFAULT_SYS_OPS) -> int:
    run_dir = resolve_run_dir(args.run)
    env_file = run_dir / "environment.json"
    if not env_file.is_file():
        print(f"ERROR: environment.json not found in {run_dir}", file=sys.stderr)
        return 1
    # Teardown must still run with another copy of the script: refusing here
    # would leave the app, the clipboard and the worktree behind.
    script_err = check_round_script(json.loads(env_file.read_text(encoding="utf-8")))
    if script_err:
        print(f"WARNING: {script_err} Tearing down anyway.", file=sys.stderr)

    env_data = json.loads(env_file.read_text(encoding="utf-8"))
    proc_file = run_dir / "app-process.json"

    exit_code = 0
    app_was_running = False

    # 1. Quit app with Cmd+Q if running, and read exit receipt from supervisor
    if proc_file.is_file():
        proc_state = json.loads(proc_file.read_text(encoding="utf-8"))
        pid = proc_state.get("pid")
        gate = proc_state.get("gate", "a")
        exit_file = run_dir / f"app-gate-{gate}-exit.json"

        if pid and sys_ops.is_pid_alive(pid):
            app_was_running = True
            print(f"Quitting app PID {pid} via Cmd+Q...")
            try:
                sys_ops.run_applescript(
                    f'tell application "System Events" to tell first application process whose unix id is {pid}\nkeystroke "q" using command down\nend tell'
                )
            except Exception:
                pass

            ec = sys_ops.wait_process_exit(pid, timeout_seconds=10.0)
            if ec is None:
                print(f"Process {pid} did not terminate within 10s. Force killing...", file=sys.stderr)
                sys_ops.kill_process(pid, signal.SIGKILL)

        # Bounded wait for supervisor to write exit receipt file
        deadline = time.time() + 5.0
        while time.time() < deadline:
            if exit_file.is_file():
                break
            time.sleep(0.1)

        if not exit_file.is_file():
            print(f"ERROR: Missing exit receipt {exit_file.name} for PID {pid}!", file=sys.stderr)
            return 1

        try:
            exit_data = json.loads(exit_file.read_text(encoding="utf-8"))
            exit_code = exit_data["exit_code"]
            print(f"[OK] Read {exit_file.name}: pid={exit_data.get('pid')} exit_code={exit_code}")
        except Exception as exc:
            print(f"ERROR: Failed to read {exit_file.name}: {exc}", file=sys.stderr)
            return 1

        if exit_code != 0:
            print(f"ERROR: App exited with non-zero exit code {exit_code}", file=sys.stderr)
            return 1

    # 2. Verify no snip desktop process for this round remains
    app_bundle_path = Path(env_data["app_bundle"])
    remaining = [
        p for p in sys_ops.list_processes()
        if str(app_bundle_path) in p.command
    ]
    if remaining:
        print(f"ERROR: Remaining processes found from this round: {remaining}", file=sys.stderr)
        return 1

    # 3. Restore clipboard
    clip_file = run_dir / "clipboard-initial.bin"
    clipboard_restored = False
    if clip_file.is_file():
        saved_bytes = clip_file.read_bytes()
        sys_ops.set_clipboard_bytes(saved_bytes)
        restored = sys_ops.get_clipboard_bytes()
        if hashlib.sha256(restored).hexdigest() != hashlib.sha256(saved_bytes).hexdigest():
            print("ERROR: Clipboard restore hash mismatch!", file=sys.stderr)
            return 1
        clipboard_restored = True
        print("[OK] Clipboard successfully restored and verified.")

    # 4. Compare real config snapshot
    real_config_dir = get_real_config_dir()
    snap_file = run_dir / "real-config-snapshot.json"
    if snap_file.is_file():
        expected_snap = json.loads(snap_file.read_text(encoding="utf-8"))
        diffs = verify_config_snapshot(real_config_dir, expected_snap)
        if diffs:
            print("ERROR: Real user config directory was modified during the round!", file=sys.stderr)
            for d in diffs:
                print(f"  - {d}", file=sys.stderr)
            return 1
        print("[OK] Real user config directory snapshot verified unchanged.")

    # 5. Unregister .app from LaunchServices
    if app_bundle_path.exists():
        sys_ops.unregister_app(app_bundle_path)
        print("[OK] Unregistered .app from LaunchServices.")

    # 6. Remove worktree
    worktree_path = Path(env_data["source_worktree"])
    if worktree_path.is_dir():
        print(f"Removing detached worktree at {worktree_path}...")
        subprocess.run(["git", "worktree", "remove", "--force", str(worktree_path)], check=False)

    # 7. Clean remote host if applicable: only what this round put there
    remote_host = env_data.get("remote_host")
    remote_cleanup: Optional[Dict[str, Any]] = None
    if remote_host:
        print(f"Cleaning remote host {remote_host}...")
        remote_cleanup = clean_remote_round(remote_host, env_data["sha"], run_dir, sys_ops)
        if not remote_cleanup.get("remote_dir_removed"):
            env_data["finish_results"] = {"remote_cleanup": remote_cleanup}
            env_file.write_text(json.dumps(env_data, indent=2), encoding="utf-8")
            why = "worker cleanup failed" if not remote_cleanup["workers_stopped"] else "a kept wrapper still runs it"
            print(f"ERROR: Remote cleanup incomplete ({why}); {remote_cleanup['remote_dir']} kept.", file=sys.stderr)
            return 1
        print("[OK] Remote host cleaned.")

    # 8. Record finish results in environment.json
    env_data["finish_results"] = {
        "finish_time": datetime.datetime.now().isoformat(),
        "app_was_running": app_was_running,
        "exit_code": exit_code,
        "clipboard_restored": clipboard_restored,
        "real_config_verified": True,
        "worktree_removed": True,
        "remote_cleanup": remote_cleanup,
    }
    env_file.write_text(json.dumps(env_data, indent=2), encoding="utf-8")
    print(f"\n[OK] Round teardown completed. Results written to {env_file}.")
    return 0


# -----------------------------------------------------------------------------
# Main CLI
# -----------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="snip-sync real-UI acceptance round operator script",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    # prepare
    p_prep = subparsers.add_parser("prepare", help="Prepare a real-UI round")
    p_prep.add_argument("--sha", required=True, help="Revision / commit SHA to build and test")
    p_prep.add_argument("--run", help="Target run directory path")
    p_prep.add_argument("--remote", help="Remote host for remote protocol validation (e.g. ubuntu)")
    p_prep.add_argument("--clean-leftovers", action="store_true", help="Clean provable earlier round leftovers")
    p_prep.add_argument(
        "--allow-running-desktop",
        action="store_true",
        help="Allow running desktop processes without failing leftover check",
    )
    p_prep.add_argument("--copy-target-from", help="Optional target directory to copy to speed up cargo build")

    # launch
    p_launch = subparsers.add_parser("launch", help="Launch the bundled app with isolated environment")
    p_launch.add_argument("--gate", required=True, choices=["a", "b"], help="Gate to launch for")
    p_launch.add_argument("--run", help="Run directory path")

    # resize
    p_resize = subparsers.add_parser("resize", help="Resize the main window to logical WxH")
    p_resize.add_argument("width", type=int, help="Logical content width")
    p_resize.add_argument("height", type=int, help="Logical content height")
    p_resize.add_argument("--run", help="Run directory path")

    # point
    p_point = subparsers.add_parser("point", help="Calculate coordinates for a control ID from latest CTRL_BOUNDS")
    p_point.add_argument("control_id", help="Exact probe control ID")
    p_point.add_argument("--run", help="Run directory path")
    p_point.add_argument(
        "--hover",
        type=float,
        metavar="SECONDS",
        help="Also move the pointer to the control's centre and stay there (tooltips); never clicks",
    )

    # title
    p_title = subparsers.add_parser("title", help="Print the exact title of the round's main window")
    p_title.add_argument("--run", help="Run directory path")

    # finish
    p_finish = subparsers.add_parser("finish", help="Tear down the round and verify cleanliness")
    p_finish.add_argument("--run", help="Run directory path")

    # _supervisor (internal detached supervisor for launch)
    p_sup = subparsers.add_parser("_supervisor", help=argparse.SUPPRESS)
    p_sup.add_argument("--gate", required=True, choices=["a", "b"], help="Gate to supervise")
    p_sup.add_argument("--run", help="Run directory path")

    return parser


def main(argv: Optional[List[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.subcommand == "prepare":
        return cmd_prepare(args)
    elif args.subcommand == "launch":
        return cmd_launch(args)
    elif args.subcommand == "_supervisor":
        return cmd_supervisor(args)
    elif args.subcommand == "resize":
        return cmd_resize(args)
    elif args.subcommand == "point":
        return cmd_point(args)
    elif args.subcommand == "title":
        return cmd_title(args)
    elif args.subcommand == "finish":
        return cmd_finish(args)
    else:
        parser.print_help()
        return 1


if __name__ == "__main__":
    sys.exit(main())
