#!/usr/bin/env python3
"""
bench_tauri_memory.py - Tauri desktop memory baseline driver for snip-sync (Linux).

Drives the real release app through tauri-driver (same WebDriver path as
crates/desktop/e2e/lib.mjs) and samples its process tree with memory_harness.py.

Profiles:
- idle:  window rendered, no repository applied.
- 1repo: one standard-dataset repo applied, commit history rendered, one commit's
         file content matched byte-exactly against `git show`, then its diff
         pane checked against `git show --unified=0` added lines.
- 15repo: reported UNSUPPORTED; the Tauri app holds a single `repo` string.

What the numbers mean (see docs/tauri-baseline-measurement.md):
- The app PID is the unique descendant of *our* tauri-driver whose exe is the
  binary under test; identity is (pid, /proc starttime). No global /proc scan.
- The sampler (a memory_harness.py subprocess) attaches as soon as that PID
  appears, so "pre-ready" covers WebView load plus the UI workload, not the
  first milliseconds after exec. processAgeAtSamplerStartSec states the gap.
- "steady" is the post-ready window only.
- Each run gets fresh XDG config/data/cache/state dirs and a private D-Bus
  session, so no persisted settings or running instance can leak in.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from typing import Any

SCRIPTS_DIR = os.path.abspath(os.path.dirname(__file__))
REPO_ROOT = os.path.dirname(SCRIPTS_DIR)
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from memory_harness import (  # noqa: E402
    HARNESS_REVISION,
    ProcessTreeSampler,
    calculate_p95,
    cleanup_process_group,
    get_system_environment,
    read_proc_starttime,
)

ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf"
XVFB_SCREEN = "1280x800x24"
# Frontend preview cap is 1 MiB; stay well under it so a refusal is never the oracle.
ORACLE_MAX_BLOB_BYTES = 256 * 1024
UNSUPPORTED_15REPO = (
    "The Tauri app has one active repository (App.tsx `const [repo, setRepo] = useState(\"\")`, "
    "commands take a single `repo: String`). 15 repositories cannot be shown simultaneously, "
    "so no comparable 15-repo measurement exists."
)


class BenchError(Exception):
    pass


# ---------------------------------------------------------------- process identity


def proc_exe(pid: int, proc_root: str = "/proc") -> str | None:
    try:
        return os.path.realpath(os.readlink(os.path.join(proc_root, str(pid), "exe")))
    except OSError:
        return None


def proc_comm(pid: int, proc_root: str = "/proc") -> str | None:
    try:
        with open(os.path.join(proc_root, str(pid), "comm")) as f:
            return f.read().strip()
    except OSError:
        return None


def identity(pid: int, proc_root: str = "/proc") -> dict[str, Any]:
    return {
        "pid": pid,
        "starttime": read_proc_starttime(pid, proc_root),
        "exe": proc_exe(pid, proc_root),
        "comm": proc_comm(pid, proc_root),
    }


def is_same_process(ident: dict[str, Any], proc_root: str = "/proc") -> bool:
    start = read_proc_starttime(ident["pid"], proc_root)
    return start is not None and start == ident["starttime"]


def reap_owned(idents: list[dict[str, Any]], grace: float = 5.0) -> list[str]:
    """Waits for exactly these (pid, starttime) processes to disappear; anything left is a problem.

    Survivors of the grace period get SIGKILL, and are still reported: a clean run leaves none.
    Processes not in idents are never signalled, so unrelated sessions are safe.
    """
    def alive(group: list[dict[str, Any]]) -> list[dict[str, Any]]:
        return [i for i in group if i["starttime"] is not None and is_same_process(i)]

    def wait(group: list[dict[str, Any]]) -> list[dict[str, Any]]:
        end = time.monotonic() + grace
        group = alive(group)
        while group and time.monotonic() < end:
            time.sleep(0.05)
            group = alive(group)
        return group

    problems = []
    survivors = wait(idents)
    for i in survivors:
        problems.append(f"owned {i['comm']} pid {i['pid']} survived teardown for {grace}s; sent SIGKILL")
        try:
            os.kill(i["pid"], signal.SIGKILL)
        except OSError:
            pass
    problems += [f"owned {i['comm']} pid {i['pid']} still alive after SIGKILL" for i in wait(survivors)]
    return problems


def descendants(owner_pid: int, proc_root: str = "/proc") -> list[int]:
    pids, _ = ProcessTreeSampler(owner_pid, None, proc_root).get_tree_pids()
    return [p for p in pids if p != owner_pid]


def find_owned_app_pid(owner_pid: int, bin_path: str, proc_root: str = "/proc") -> int | None:
    """The unique descendant of owner_pid running bin_path; None if not started yet.

    Only processes we spawned are considered; an unrelated snip-sync elsewhere on
    the machine can never be selected.
    """
    target = os.path.realpath(bin_path)
    matches = [p for p in descendants(owner_pid, proc_root) if proc_exe(p, proc_root) == target]
    if len(matches) > 1:
        raise BenchError(f"More than one owned process runs {target}: {matches}")
    return matches[0] if matches else None


def process_age_sec(pid: int) -> float | None:
    start = read_proc_starttime(pid)
    if start is None:
        return None
    with open("/proc/uptime") as f:
        uptime = float(f.read().split()[0])
    return round(uptime - start / os.sysconf("SC_CLK_TCK"), 3)


def sample_processes(pids: list[int]) -> dict[str, Any]:
    """One-shot RSS/PSS of the given PIDs, each labelled with its identity."""
    sampler = ProcessTreeSampler(os.getpid(), None)
    procs = []
    for pid in pids:
        ident = identity(pid)
        rss, pss, _, _ = sampler.sample_process_memory(pid)
        if ident["starttime"] is None or not rss:
            continue  # exited or zombie
        procs.append({**ident, "rssBytes": rss, "pssBytes": pss})
    total_rss = sum(p["rssBytes"] for p in procs)
    pss_all = all(p["pssBytes"] is not None for p in procs)
    total_pss = sum(p["pssBytes"] for p in procs) if pss_all else None
    return {
        "processes": procs,
        "rssMib": round(total_rss / 1048576, 2),
        "pssMib": round(total_pss / 1048576, 2) if total_pss is not None else None,
    }


GRAPHICS_LIB_HINTS = ("dri", "gallium", "libEGL", "libGLX", "libGL.so", "libvulkan", "libgbm", "llvmpipe", "swrast")


def loaded_graphics_libs(pid: int) -> list[str]:
    """GL/DRI libraries actually mapped by pid: evidence of the render backend, not a guess."""
    try:
        with open(f"/proc/{pid}/maps") as f:
            paths = {line.split(None, 5)[5].strip() for line in f if len(line.split(None, 5)) == 6}
    except OSError:
        return []
    return sorted(p for p in paths if p.startswith("/") and any(h in os.path.basename(p) for h in GRAPHICS_LIB_HINTS))


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# ---------------------------------------------------------------- git oracle


def git(repo: str, *args: str, text: bool = True) -> Any:
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR")}
    return subprocess.check_output(["git", "-C", repo, *args], env=env, text=text)


def changed_lines(patch: str) -> list[str]:
    """Text of every added and removed line of a unified diff (headers excluded)."""
    return [
        l[1:] for l in patch.splitlines()
        if l[:1] in ("+", "-") and not l.startswith(("+++", "---")) and l[1:].strip()
    ]


def commit_oracle(repo: str, sha: str) -> dict[str, Any] | None:
    """Expected blob and diff for the first added/modified small text file of a non-merge commit."""
    parents = git(repo, "rev-list", "--parents", "-n", "1", sha).split()[1:]
    if len(parents) > 1:
        return None
    changes = git(repo, "diff-tree", "--root", "--no-commit-id", "-r", "--name-status", "-z", sha).split("\0")
    for status, path in zip(changes[0::2], changes[1::2]):
        if status not in ("A", "M"):
            continue
        if int(git(repo, "cat-file", "-s", f"{sha}:{path}")) > ORACLE_MAX_BLOB_BYTES:
            continue
        blob: bytes = git(repo, "show", f"{sha}:{path}", text=False)
        if b"\0" in blob:
            continue
        try:
            content = blob.decode("utf-8")
        except UnicodeDecodeError:
            continue
        lines = changed_lines(git(repo, "show", "--format=", "--unified=0", sha, "--", path))
        if not lines:
            continue
        return {
            "sha": sha,
            "path": path,
            "content": content,
            "contentSha256": hashlib.sha256(blob).hexdigest(),
            "changedLines": lines[:20],
        }
    return None


def check_source_content(actual: Any, oracle: dict[str, Any]) -> None:
    """Fails unless the rendered content is exactly the committed blob."""
    if not isinstance(actual, str) or actual == "":
        raise BenchError(f"source-content for {oracle['path']} is {actual!r}: preview not loaded")
    if actual != oracle["content"]:
        raise BenchError(
            f"source-content for {oracle['sha'][:12]}:{oracle['path']} does not match git show "
            f"(rendered {len(actual)} chars sha256 {hashlib.sha256(actual.encode()).hexdigest()[:12]}, "
            f"expected {len(oracle['content'])} chars sha256 {oracle['contentSha256'][:12]})"
        )


def missing_diff_lines(pane_text: Any, lines: list[str]) -> list[str]:
    if not isinstance(pane_text, str):
        return list(lines)
    return [l for l in lines if l not in pane_text]


# ---------------------------------------------------------------- WebDriver


def isolated_env(iso_root: str) -> tuple[dict[str, str], str]:
    """Environment with fresh XDG dirs under iso_root, plus a private D-Bus config file.

    The bus has no <servicedir>: nothing is auto-activated, so portal, keyring and a11y
    lookups fail fast instead of starting desktop services (or blocking 25 s on them).
    """
    env = dict(os.environ)
    for name in ("CONFIG", "DATA", "CACHE", "STATE"):
        path = os.path.join(iso_root, name.lower())
        os.makedirs(path)
        env[f"XDG_{name}_HOME"] = path
    bus_config = os.path.join(iso_root, "session-bus.xml")
    with open(bus_config, "w") as f:
        f.write(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" "
            "\"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n"
            f"<busconfig><type>session</type><listen>unix:tmpdir={iso_root}</listen>"
            "<auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>"
            "<allow eavesdrop=\"true\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>\n"
        )
    return env, bus_config


def spawn_attached_harness(app: dict[str, Any], run_dir: str, ready_file: str, marker: str, steady: float,
                           interval: float, label: str, log: Any, readiness_timeout: float = 300.0,
                           sampler_ready_file: str | None = None,
                           expected_exe: str | None = None) -> subprocess.Popen:
    """memory_harness.py attached to app's (pid, starttime), writing its report into run_dir.

    Passing expected_exe does not by itself claim a launch phase. The harness claims launch
    only after it publishes sampler_ready_file while the exe is still not expected_exe and
    then observes that transition.
    """
    cmd = [
        sys.executable, "-B", os.path.join(SCRIPTS_DIR, "memory_harness.py"),
        "--attach-pid", str(app["pid"]), "--attach-starttime", str(app["starttime"]),
        "--ready-file", ready_file, "--ready-marker", marker,
        "--readiness-timeout", str(readiness_timeout), "--steady-seconds", str(steady),
        "--sample-interval", str(interval), "--profile-label", label,
        "--out-dir", run_dir,
    ]
    if sampler_ready_file:
        cmd.extend(["--sampler-ready-file", sampler_ready_file])
    if expected_exe:
        cmd.extend(["--expected-exe", expected_exe])
    return subprocess.Popen(cmd, stdout=log, stderr=log)


class Driver:
    """One tauri-driver + app, owned by this process and isolated from the user session."""

    def __init__(self, bin_path: str, log_path: str):
        self.bin_path = os.path.realpath(bin_path)
        self.port = free_port()
        self.native_port = free_port()
        self.log = open(log_path, "wb")
        self.iso_root = tempfile.mkdtemp(prefix="snip-bench-xdg-")
        env, bus_config = isolated_env(self.iso_root)
        self.isolation = {
            "xdgRoot": self.iso_root,
            "xdgVars": ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"],
            "dbus": "private session bus (dbus-run-session, no service activation: no portal/keyring/a11y/tray host)",
            "display": f"private Xvfb via xvfb-run -a -s '-screen 0 {XVFB_SCREEN}'",
        }
        self.cmd = [
            "xvfb-run", "-a", "-s", f"-screen 0 {XVFB_SCREEN}",
            "dbus-run-session", f"--config-file={bus_config}", "--",
            "tauri-driver", "--port", str(self.port), "--native-port", str(self.native_port),
        ]
        self.proc = subprocess.Popen(self.cmd, stdout=self.log, stderr=self.log, env=env, start_new_session=True)
        self.sid: str | None = None
        self.app: dict[str, Any] | None = None
        # Every (pid, starttime) ever seen under this driver; teardown waits for all of them.
        self.owned: list[dict[str, Any]] = []

    def _http(self, method: str, path: str, body: Any = None, timeout: float = 30.0) -> Any:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}{path}", data=data, method=method,
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return json.loads(resp.read().decode())["value"]
        except urllib.error.HTTPError as e:
            raise BenchError(f"WebDriver {method} {path}: HTTP {e.code} {e.read().decode(errors='replace')[:500]}") from e

    def wait_driver(self, timeout: float = 15.0) -> None:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if self.proc.poll() is not None:
                raise BenchError(f"tauri-driver wrapper exited with {self.proc.returncode}")
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/status", timeout=2.0):
                    return
            except OSError:
                time.sleep(0.1)
        raise BenchError(f"tauri-driver not ready on port {self.port} within {timeout}s")

    def start_session(self, on_app_pid: Any, timeout: float = 120.0) -> None:
        """Creates the session; calls on_app_pid(identity) as soon as the owned app PID appears."""
        result: dict[str, Any] = {}

        def create() -> None:
            try:
                caps = {"capabilities": {"alwaysMatch": {"tauri:options": {"application": self.bin_path}}}}
                # WebView cold start is slow on loaded hosts; lib.mjs allows 120 s as well.
                result["sid"] = self._http("POST", "/session", caps, timeout=timeout)["sessionId"]
            except Exception as e:  # noqa: BLE001 - re-raised on the main thread
                result["error"] = e

        worker = threading.Thread(target=create, daemon=True)
        worker.start()
        end = time.monotonic() + timeout
        while self.app is None and time.monotonic() < end:
            pid = find_owned_app_pid(self.proc.pid, self.bin_path)
            if pid is not None:
                ident = identity(pid)
                if ident["starttime"] is not None:
                    self.app = ident
                    self.app["ageAtDiscoverySec"] = process_age_sec(pid)
                    self.app["discoveryMonotonic"] = time.monotonic()
                    on_app_pid(self.app)
                    break
            if "error" in result:
                break
            time.sleep(0.01)
        worker.join(timeout=max(0.0, end - time.monotonic()))
        if "error" in result:
            raise BenchError(f"session creation failed: {result['error']}")
        if "sid" not in result:
            raise BenchError(f"session not created within {timeout}s")
        self.sid = result["sid"]
        if self.app is None:
            raise BenchError(f"no owned process running {self.bin_path} under tauri-driver PID {self.proc.pid}")

    def assert_app_alive(self) -> None:
        if self.app is None or not is_same_process(self.app):
            raise BenchError(f"app process {self.app} exited or its PID was reused")

    def wd(self, method: str, sub: str, body: Any = None) -> Any:
        return self._http(method, f"/session/{self.sid}{sub}", body)

    def js(self, script: str, *args: Any) -> Any:
        return self.wd("POST", "/execute/sync", {"script": script, "args": list(args)})

    def until(self, what: str, fn: Any, timeout: float = 60.0) -> Any:
        end = time.monotonic() + timeout
        last: Any = None
        while time.monotonic() < end:
            try:
                v = fn()
                if v:
                    return v
            except BenchError as e:
                last = e
            time.sleep(0.2)
        raise BenchError(f"timed out waiting for {what}" + (f": {last}" if last else ""))

    def click(self, css: str) -> None:
        el = self.until(css, lambda: self.wd("POST", "/element", {"using": "css selector", "value": css})[ELEMENT_KEY])
        self.wd("POST", f"/element/{el}/click", {})

    def attr(self, testid: str, name: str) -> Any:
        return self.js(f"const e = document.querySelector('[data-testid=\"{testid}\"]'); return e && e.getAttribute('{name}')")

    def screenshot(self, path: str) -> str:
        png = base64.b64decode(self.wd("GET", "/screenshot"))
        if png[:8] != b"\x89PNG\r\n\x1a\n":
            raise BenchError("WebDriver screenshot is not a PNG")
        with open(path, "wb") as f:
            f.write(png)
        return path

    def app_tree_pids(self) -> list[int]:
        return [self.app["pid"], *descendants(self.app["pid"])] if self.app else []

    def controller_pids(self) -> list[int]:
        app = set(self.app_tree_pids())
        return [self.proc.pid, *(p for p in descendants(self.proc.pid) if p not in app)]

    def remember_owned(self) -> None:
        """Records the current driver tree so teardown can follow processes that later leave it."""
        known = {(i["pid"], i["starttime"]) for i in self.owned}
        for p in [self.proc.pid, *descendants(self.proc.pid)]:
            ident = identity(p)
            if ident["starttime"] is not None and (ident["pid"], ident["starttime"]) not in known:
                self.owned.append(ident)

    def stop(self) -> list[str]:
        """Tears down everything this driver owns and waits for it to be gone; returns problems."""
        problems: list[str] = []
        self.remember_owned()
        if self.sid:
            try:
                self._http("DELETE", f"/session/{self.sid}", timeout=10.0)
            except Exception as e:  # noqa: BLE001
                problems.append(f"session delete: {e}")
        try:
            cleanup_process_group(os.getpgid(self.proc.pid), self.proc)
        except Exception as e:  # noqa: BLE001
            problems.append(f"driver process group: {e}")
        # Helpers may have left the group (or been reparented); follow them by identity.
        problems += reap_owned(self.owned)
        self.log.close()
        shutil.rmtree(self.iso_root, ignore_errors=True)
        return problems


# ---------------------------------------------------------------- UI workloads


def wait_idle(d: Driver) -> dict[str, Any]:
    d.until("tauri:// document with repo-path", lambda: str(d.wd("GET", "/url")).startswith(("tauri://", "http://tauri.localhost"))
            and d.js("return !!document.querySelector('[data-testid=\"repo-path\"]')"))
    applied = d.attr("repo-path", "data-applied-path")
    rows = d.js("return document.querySelectorAll('[data-commit]').length")
    if applied != "" or rows != 0:
        raise BenchError(f"idle UI is not empty: applied repo {applied!r}, {rows} commit rows")
    return {"appliedRepo": applied, "renderedCommitRows": rows}


def load_repo_preview(d: Driver, repo: str, shots: str) -> dict[str, Any]:
    wait_idle(d)
    # Same input path as lib.mjs setRepo (WebKitWebDriver under Xvfb mangles typed capitals).
    el = d.wd("POST", "/element", {"using": "css selector", "value": '[data-testid="repo-path"]'})[ELEMENT_KEY]
    d.js(
        'const [input, value] = arguments;'
        'Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);'
        'input.dispatchEvent(new Event("input", { bubbles: true }));',
        {ELEMENT_KEY: el}, repo,
    )
    d.wd("POST", f"/element/{el}/value", {"text": ""})
    d.until("repo path to apply", lambda: d.attr("repo-path", "data-applied-path") == repo)
    d.click('[role="tab"][data-key="commits"]')
    rendered = d.until("commit rows", lambda: d.js("return [...document.querySelectorAll('[data-commit]')].map(e => e.dataset.commit)"), 120.0)

    # Pick the first rendered commit that has a checkable text change; nothing is fabricated.
    oracle = None
    for sha in rendered[:50]:
        oracle = commit_oracle(repo, sha)
        if oracle:
            break
    if oracle is None:
        raise BenchError(f"none of the first {min(50, len(rendered))} rendered commits has an A/M text file to verify")

    d.click(f'[data-commit="{oracle["sha"]}"] span')
    d.click(f'[data-testid="preview-file-{oracle["path"]}"]')
    d.until("preview of the oracle file", lambda: d.attr("source-preview", "data-path") == oracle["path"])
    d.click('[data-testid="preview-content"]')
    try:
        d.until("exact source content", lambda: d.js(
            "return document.querySelector('[data-testid=\"source-content\"]')?.textContent") == oracle["content"])
    except BenchError:
        check_source_content(d.js("return document.querySelector('[data-testid=\"source-content\"]')?.textContent ?? null"), oracle)
        raise
    content_shot = d.screenshot(os.path.join(shots, "ready-1repo-content.png"))

    d.click('[data-testid="preview-diff"]')
    deep_text = (
        "const walk = (n) => { let t = ''; for (const c of n.childNodes) {"
        " if (c.nodeType === 3) t += c.data; else if (c.nodeType === 1) { if (c.shadowRoot) t += walk(c.shadowRoot) + '\\n'; t += walk(c); } }"
        " return t; }; const root = document.querySelector('[data-testid=\"source-preview\"]'); return root ? walk(root) : null;"
    )
    try:
        d.until("diff pane with every changed line", lambda: not missing_diff_lines(d.js(deep_text), oracle["changedLines"]))
    except BenchError as e:
        raise BenchError(f"diff pane misses {missing_diff_lines(d.js(deep_text), oracle['changedLines'])[:3]}") from e
    diff_shot = d.screenshot(os.path.join(shots, "ready-1repo-diff.png"))

    return {
        "appliedRepo": repo,
        "renderedCommitRows": len(rendered),
        "oracle": {k: v for k, v in oracle.items() if k != "content"},
        "sourceContentMatchedGitShow": True,
        "diffLinesChecked": len(oracle["changedLines"]),
        "diffAssertion": "every +/- line of `git show --unified=0 <sha> -- <path>` (max 20) is contained in the rendered diff pane text; containment, not byte-exact (the diff renderer adds line numbers and markup)",
        "screenshots": [content_shot, diff_shot],
    }


def ui_environment(d: Driver) -> dict[str, Any]:
    """Window facts observed in the live app, read after sampling ended."""
    info = {"kind": "observed in the live app after the steady window"}
    info |= d.js(
        "return { innerWidth, innerHeight, devicePixelRatio, visibilityState: document.visibilityState,"
        " userAgent: navigator.userAgent };"
    )
    info["windowRect"] = d.wd("GET", "/window/rect")
    # WebKit masks WebGL renderer strings (it reports "Apple GPU"), so read mapped libraries instead.
    info["graphicsLibsMapped"] = {
        f"{ident['comm']}:{ident['pid']}": loaded_graphics_libs(ident["pid"])
        for ident in (identity(p) for p in d.app_tree_pids())
    }
    info["graphicsEnv"] = {k: os.environ.get(k) for k in ("LIBGL_ALWAYS_SOFTWARE", "WEBKIT_DISABLE_COMPOSITING_MODE", "WEBKIT_DISABLE_DMABUF_RENDERER", "GALLIUM_DRIVER")}
    return info


# ---------------------------------------------------------------- one run


def finalize_run(result: dict[str, Any], error: str | None) -> dict[str, Any]:
    """COMPLETED only with a measurement, no error and a clean teardown; everything collected is kept."""
    problems = result.get("cleanupProblems") or []
    if error is None and not problems and "measurement" in result:
        result["status"] = "COMPLETED"
    else:
        result["status"] = "FAILED"
        reasons = [error] if error else []
        if problems:
            reasons.append("cleanup: " + "; ".join(problems))
        result["error"] = " | ".join(reasons) or "no measurement collected"
    return result


def run_profile(profile: str, bin_path: str, repo: str, run_dir: str, steady: float, interval: float) -> dict[str, Any]:
    os.makedirs(run_dir, exist_ok=True)
    result: dict[str, Any] = {"profile": profile, "runDir": run_dir}
    error: str | None = None
    d: Driver | None = None
    try:
        d = Driver(bin_path, os.path.join(run_dir, "driver.log"))
        result["driverCommand"] = d.cmd
        result["isolation"] = d.isolation
        drive(d, profile, repo, run_dir, steady, interval, result)
    except Exception as e:  # noqa: BLE001 - recorded; the run fails after teardown
        error = f"{type(e).__name__}: {e}"
    finally:
        # Also runs on SystemExit (SIGTERM), which then propagates.
        result["cleanupProblems"] = d.stop() if d is not None else []
    return finalize_run(result, error)


def drive(d: Driver, profile: str, repo: str, run_dir: str, steady: float, interval: float, result: dict[str, Any]) -> None:
    """Runs one measured session, filling result as it goes so a failure keeps what was collected."""
    result["samplerAttachment"] = {
        "mode": "late-attach",
        "launchPhaseClaimed": False,
        "detail": "Sampler starts after the owned app PID exists. pre-ready covers WebView load and the UI workload; it is not a launch phase.",
    }
    ready_file = os.path.join(run_dir, f"ready-{uuid.uuid4().hex}.signal")
    marker = f"[READY:{profile.upper()}:{uuid.uuid4().hex}]"
    harness: subprocess.Popen | None = None
    harness_log = open(os.path.join(run_dir, "harness.log"), "wb")
    try:
        d.wait_driver()

        def attach(app: dict[str, Any]) -> None:
            nonlocal harness
            harness = spawn_attached_harness(app, run_dir, ready_file, marker, steady, interval,
                                             f"Tauri release {profile}", harness_log)

        d.start_session(attach)
        d.remember_owned()
        result["app"] = d.app
        if profile == "idle":
            result["ui"] = wait_idle(d)
            result["ui"]["screenshots"] = [d.screenshot(os.path.join(run_dir, "ready-idle.png"))]
        else:
            result["ui"] = load_repo_preview(d, repo, run_dir)
        d.assert_app_alive()
        d.remember_owned()
        result["appTreeAtReady"] = [identity(p) for p in d.app_tree_pids()]

        tmp = ready_file + ".tmp"
        with open(tmp, "w") as f:
            f.write(marker + "\n")
        os.replace(tmp, ready_file)

        code = harness.wait(timeout=steady + 120)
        if code != 0:
            raise BenchError(f"memory_harness exited {code}; see {run_dir}/harness.log")
        d.assert_app_alive()
        with open(os.path.join(run_dir, "benchmark_report.json")) as f:
            result["measurement"] = json.load(f)["results"][0]
        # CLOCK_MONOTONIC is shared across processes, so this is the app's age at the first sample.
        result["processAgeAtSamplerStartSec"] = round(
            d.app["ageAtDiscoverySec"] + result["measurement"]["timestamps"]["startMonotonic"] - d.app["discoveryMonotonic"], 3
        )
        d.remember_owned()
        result["appTreeAtEnd"] = [identity(p) for p in d.app_tree_pids()]
        result["controllerAtEnd"] = sample_processes(d.controller_pids())
        result["benchPythonAtEnd"] = sample_processes([os.getpid()])
        result["uiEnvironment"] = ui_environment(d)
    finally:
        if harness is not None and harness.poll() is None:
            harness.kill()
            harness.wait()
        harness_log.close()
        for leftover in (ready_file, ready_file + ".tmp"):
            if os.path.exists(leftover):
                os.remove(leftover)


# ---------------------------------------------------------------- metadata and report


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_build_receipt(bin_path: str, receipt_arg: str | None) -> dict[str, Any] | None:
    """Provenance for a build receipt, or None when the caller did not pass one and no sidecar exists.

    origin is "user-supplied" only for --build-receipt. A sidecar next to the binary is "sidecar".
    A sha256 field that is not the on-disk hash of bin_path is rejected before any benchmark work.
    """
    if receipt_arg:
        path = receipt_arg
        origin = "user-supplied"
        if not os.path.isfile(path):
            raise ValueError(f"user-supplied build receipt not found: {path}")
    else:
        path = bin_path + ".receipt.json"
        if not os.path.isfile(path):
            return None
        origin = "sidecar"
    try:
        with open(path, encoding="utf-8") as f:
            document = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        raise ValueError(f"build receipt {path} is not readable JSON: {e}") from e
    if not isinstance(document, dict):
        raise ValueError(f"build receipt {path} must be a JSON object")
    actual = sha256_file(bin_path)
    claimed = document.get("sha256")
    checked = None
    if claimed is not None:
        if not isinstance(claimed, str) or claimed.strip().lower() != actual:
            raise ValueError(
                f"build receipt sha256 {claimed!r} does not match {bin_path} ({actual})"
            )
        checked = True
    return {
        "origin": origin,
        "path": os.path.abspath(path),
        "sha256MatchesBinary": checked,
        "binarySha256": actual,
        "document": document,
    }


def run_text(*cmd: str) -> str | None:
    try:
        return subprocess.check_output(list(cmd), text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


TAURI_CONF = os.path.join(REPO_ROOT, "crates", "desktop", "src-tauri", "tauri.conf.json")


def tauri_config_source() -> dict[str, Any]:
    """Window config as written in this tree. The binary embeds its config at build time, so this
    only describes the binary if it was built from this tree; the live window is in uiEnvironment."""
    try:
        with open(TAURI_CONF) as f:
            windows = json.load(f)["app"]["windows"]
    except (OSError, ValueError, KeyError) as e:
        return {"path": TAURI_CONF, "error": str(e)}
    keys = ("label", "width", "height", "minWidth", "minHeight", "visible")
    return {
        "path": TAURI_CONF,
        "gitBlob": run_text("git", "-C", REPO_ROOT, "hash-object", TAURI_CONF),
        "kind": "configured (source file), not observed",
        "windows": [{k: w.get(k) for k in keys} for w in windows],
    }


def collect_metadata(
    bin_path: str,
    dataset: str,
    build_command: str | None,
    build_profile: str | None,
    build_receipt: dict[str, Any] | None = None,
) -> dict[str, Any]:
    with open(os.path.join(dataset, "workload_manifest.json")) as f:
        manifest = json.load(f)
    st = os.stat(bin_path)
    meta: dict[str, Any] = {
        "harnessRevision": HARNESS_REVISION,
        "gitHead": run_text("git", "-C", REPO_ROOT, "rev-parse", "HEAD"),
        "gitDirtyPaths": (run_text("git", "-C", REPO_ROOT, "status", "--porcelain") or "").splitlines(),
        "lastDesktopOrCoreCommit": run_text("git", "-C", REPO_ROOT, "log", "-1", "--format=%H %cI", "--", "crates/desktop", "crates/core", "Cargo.lock"),
        "binary": {
            "path": bin_path,
            "sha256": sha256_file(bin_path),
            "sizeBytes": st.st_size,
            "mtimeUtc": datetime.datetime.fromtimestamp(st.st_mtime, datetime.timezone.utc).isoformat(),
            "buildCommand": build_command or "unknown (not built by this invocation; pass --build-command)",
            "buildProfile": build_profile or "unknown (pass --build-profile)",
            "buildReceipt": build_receipt,
        },
        "webkit2gtk": run_text("pkg-config", "--modversion", "webkit2gtk-4.1") or "unknown",
        "tauriDriver": shutil.which("tauri-driver"),
        "webkitWebDriver": shutil.which("WebKitWebDriver"),
        "tauriConfigSource": tauri_config_source(),
        "system": get_system_environment(),
        "dataset": {
            "path": dataset,
            "workloadRevision": manifest["workloadRevision"],
            "seed": manifest["seed"],
            "preset": manifest["preset"],
            "parameters": manifest["parameters"],
            "summary": manifest["summary"],
        },
        "cacheProvenance": {
            "processState": "cold",
            "processIsolation": "fresh process tree, private XDG dirs, private D-Bus session per run",
            "filesystemCache": "uncontrolled",
            "filesystemColdDemonstrated": False,
            "filesystemColdStatus": "UNSUPPORTED (filesystem-cold start requires kernel drop_caches; host page cache is uncontrolled)",
            "runRepeatability": "unpurged host page cache; cold process memory",
        },
        "coldStart": "UNSUPPORTED (process-cold only; filesystem cache uncontrolled; drop_caches not performed)",
        "samplerAttachment": {
            "mode": "late-attach",
            "launchPhaseClaimed": False,
            "detail": "Sampler starts after the owned app PID exists. pre-ready is not a launch phase.",
        },
    }
    return meta


def mib(values: list[float]) -> dict[str, Any]:
    return {
        "n": len(values),
        "median": round(statistics.median(values), 2),
        "p95": round(calculate_p95(values), 2),
        "worst": round(max(values), 2),
    }


def phase_peak_rss_mib(run_dir: str, phase: str) -> float | None:
    """Max sampled tree RSS among raw samples of exactly this phase; None if there are none."""
    with open(os.path.join(run_dir, "raw_samples.jsonl")) as f:
        values = [s["totalRssMib"] for s in map(json.loads, f) if s["phase"] == phase]
    return max(values) if values else None


def summarize(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """Cross-run statistics over COMPLETED runs only; each field names the sample phase it covers."""
    done = [r for r in runs if r.get("status") == "COMPLETED"]
    if not done:
        return {"completedRuns": 0, "failedRuns": len(runs)}
    ok = [r["measurement"] for r in done]
    out = {
        "completedRuns": len(done),
        "failedRuns": len(runs) - len(done),
        "steadyRssMedianMib": mib([m["steadyMetrics"]["rssMedianMib"] for m in ok]),
        "steadySampledPeakRssMib": mib([m["steadyMetrics"]["rssMaxMib"] for m in ok]),
        "attachToEndSampledPeakRssMib": mib([m["overallPeak"]["sampledPeakRssMib"] for m in ok]),
        "appRootVmHwmMib": mib([m["mainProcessVmHwm"]["mib"] for m in ok if m["mainProcessVmHwm"]["mib"] is not None]),
        "attachToReadySec": mib([m["timestamps"]["attachToReadySec"] for m in ok]),
        "steadyProcessCount": sorted({m["processTree"]["steadyMedianProcessCount"] for m in ok}),
        "processAgeAtSamplerStartSec": mib([r["processAgeAtSamplerStartSec"] for r in done]),
    }
    if all(m["steadyMetrics"]["pssMedianMib"] is not None for m in ok):
        out["steadyPssMedianMib"] = mib([m["steadyMetrics"]["pssMedianMib"] for m in ok])
    launch = [phase_peak_rss_mib(r["runDir"], "launch") for r in done]
    if len(launch) == len(done) and all(p is not None for p in launch):
        out["launchSampledPeakRssMib"] = mib([p for p in launch if p is not None])
    pre = [phase_peak_rss_mib(r["runDir"], "pre-ready") for r in done]
    if len(pre) == len(done) and all(p is not None for p in pre):
        out["preReadySampledPeakRssMib"] = mib(pre)
    return out


def markdown(report: dict[str, Any]) -> str:
    meta = report["metadata"]
    lines = [
        "# Tauri memory baseline (Linux, WebKitGTK)",
        "",
        f"- Date (UTC): `{report['timestampUtc']}`",
        f"- Worktree HEAD: `{meta['gitHead']}` (dirty paths: {len(meta['gitDirtyPaths'])})",
        f"- Binary: `{meta['binary']['path']}` sha256 `{meta['binary']['sha256']}`",
        f"- Build: `{meta['binary']['buildCommand']}` profile `{meta['binary']['buildProfile']}`",
        (
            f"- Build receipt ({meta['binary']['buildReceipt']['origin']}): `{meta['binary']['buildReceipt']['path']}`"
            if meta["binary"].get("buildReceipt")
            else "- Build receipt: none recorded"
        ),
        f"- WebKitGTK {meta['webkit2gtk']}; {meta['system']['os']} {meta['system']['kernel']}; {meta['system']['cpu']}",
        f"- Dataset: `{meta['dataset']['path']}` rev {meta['dataset']['workloadRevision']} seed {meta['dataset']['seed']}: "
        f"{meta['dataset']['summary']['totalRepos']} repos, {meta['dataset']['summary']['totalCommits']} commits",
        f"- Runs per profile requested: {report['runsPerProfile']}; steady window {report['steadySeconds']} s at {report['sampleIntervalSec']} s",
        "",
        "## Summary (MiB, across runs: median / p95 / worst)",
        "",
        "Peaks are maxima of 50 ms samples within the named phase only. This driver attaches late: there is no launch phase and no filesystem-cold peak.",
        "Observed viewport is per run (`uiEnvironment` innerWidth/innerHeight and windowRect), not a screen flag.",
        "",
        "| Profile | Runs OK | Steady RSS median | Steady PSS median | Pre-ready sampled peak RSS | Steady sampled peak RSS | App root VmHWM | Attach→ready s |",
        "| --- | :-: | --- | --- | --- | --- | --- | --- |",
    ]
    fmt = lambda s: f"{s['median']} / {s['p95']} / {s['worst']} (n={s['n']})" if s else "n/a"  # noqa: E731
    for name, prof in report["profiles"].items():
        if prof.get("status") == "UNSUPPORTED":
            lines.append(f"| {name} | UNSUPPORTED | – | – | – | – | – | – |")
            continue
        s = prof["summary"]
        if not s.get("completedRuns"):
            lines.append(f"| {name} | 0/{len(prof['runs'])} | – | – | – | – | – | – |")
            continue
        lines.append(
            f"| {name} | {s['completedRuns']}/{len(prof['runs'])} | {fmt(s['steadyRssMedianMib'])} | {fmt(s.get('steadyPssMedianMib'))} | "
            f"{fmt(s.get('preReadySampledPeakRssMib'))} | {fmt(s['steadySampledPeakRssMib'])} | {fmt(s.get('appRootVmHwmMib'))} | {fmt(s['attachToReadySec'])} |"
        )
    lines += ["", "15repo: " + UNSUPPORTED_15REPO]
    if report.get("provenance"):
        lines += ["", "Provenance: " + report["provenance"]]
    lines += ["", "## Runs", ""]
    for name, prof in report["profiles"].items():
        for r in prof.get("runs", []):
            if r.get("status") != "COMPLETED":
                lines.append(f"- {name} `{r['runDir']}`: FAILED {r.get('error')}; cleanup problems {r.get('cleanupProblems') or 'none'}")
                continue
            m, ui = r["measurement"], r["ui"]
            detail = f"oracle {ui['oracle']['sha'][:12]}:{ui['oracle']['path']} content==git show, {ui['diffLinesChecked']} diff lines" if "oracle" in ui else "idle, no repo applied"
            env = r["uiEnvironment"]
            libs = sorted({os.path.basename(p) for v in env["graphicsLibsMapped"].values() for p in v})
            lines.append(
                f"- {name} `{r['runDir']}`: steady RSS {m['steadyMetrics']['rssMedianMib']} PSS {m['steadyMetrics']['pssMedianMib']} MiB, "
                f"{m['processTree']['steadyMedianProcessCount']} procs, app PID {r['app']['pid']} age at first sample {r['processAgeAtSamplerStartSec']} s; "
                f"{detail}; controller RSS {r['controllerAtEnd']['rssMib']} PSS {r['controllerAtEnd']['pssMib']} MiB; "
                f"window {env['innerWidth']}x{env['innerHeight']}@{env['devicePixelRatio']} {env['visibilityState']}; "
                f"GL libs mapped {libs or 'none'}; cleanup problems {r['cleanupProblems'] or 'none'}"
            )
    return "\n".join(lines) + "\n"


PHASE_DEFINITIONS = {
    "pre-ready": "late attach: samples from when the owned app PID already exists (processAgeAtSamplerStartSec after exec) until the ready marker. WebView load plus the UI workload. Not a launch phase and not the first instruction after exec.",
    "steady": "raw samples in the post-ready window of steadySeconds with no UI input",
    "launch": "NOT CLAIMED. This driver has no pre-exec sampler gate and does not observe an exec transition.",
    "preReadySampledPeakRssMib": "max tree RSS over pre-ready samples only",
    "steadySampledPeakRssMib": "max tree RSS over steady samples only",
    "attachToEndSampledPeakRssMib": "max tree RSS over pre-ready and steady samples",
    "coldStartPeak": "UNMEASURED: the first processAgeAtSamplerStartSec of the app's life is not sampled; page cache is not dropped between runs",
}


def write_report(out_dir: str, report: dict[str, Any], stem: str = "tauri_baseline_report") -> None:
    with open(os.path.join(out_dir, f"{stem}.json"), "w") as f:
        json.dump(report, f, indent=2)
    with open(os.path.join(out_dir, f"{stem}.md"), "w") as f:
        f.write(markdown(report))


def regenerate(src_dir: str) -> int:
    """Recomputes summaries of an earlier report from its runs' raw_samples.jsonl.

    Writes tauri_baseline_report.regenerated.{json,md} next to the original, which is left untouched.
    """
    with open(os.path.join(src_dir, "tauri_baseline_report.json")) as f:
        report = json.load(f)
    notes = []
    for name, prof in report["profiles"].items():
        if "runs" in prof:
            prof["summary"] = summarize(prof["runs"])
    if "15repo" not in report["profiles"]:
        report["profiles"]["15repo"] = {"status": "UNSUPPORTED", "reason": UNSUPPORTED_15REPO}
        notes.append("the original run predates 15repo serialization; its UNSUPPORTED entry was added at regeneration, nothing was measured")
    report["phaseDefinitions"] = PHASE_DEFINITIONS
    report["provenance"] = (
        f"regenerated {datetime.datetime.now(datetime.timezone.utc).isoformat()} by harness {HARNESS_REVISION} from "
        f"{src_dir}/tauri_baseline_report.json and each run's raw_samples.jsonl (original report untouched)"
        + ("; " + "; ".join(notes) if notes else "")
    )
    write_report(src_dir, report, "tauri_baseline_report.regenerated")
    print(f"report: {src_dir}/tauri_baseline_report.regenerated.md")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--regenerate", metavar="OUT_DIR", help="Recompute summaries of an existing out-dir from its raw samples, then exit.")
    ap.add_argument("--bin", default=os.path.join(REPO_ROOT, "target", "release", "snip-sync"))
    ap.add_argument("--build-command", default=None, help="Exact command that produced --bin, recorded verbatim.")
    ap.add_argument("--build-profile", default=None, help="Cargo profile of --bin (e.g. release), recorded verbatim.")
    ap.add_argument("--build-receipt", default=None, help="User-supplied build receipt JSON. A sha256 field must match --bin or the run is refused.")
    ap.add_argument("--workspace", help="Standard workload dir (with workload_manifest.json).")
    ap.add_argument("--repo", default="repo-01-core", help="Repo under --workspace for the 1repo profile.")
    ap.add_argument("--out-dir")
    ap.add_argument("--profile", choices=["all", "idle", "1repo"], default="all")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--steady-seconds", type=float, default=30.0)
    ap.add_argument("--sample-interval", type=float, default=0.05)
    args = ap.parse_args(argv)
    if args.regenerate:
        return regenerate(os.path.abspath(args.regenerate))
    if not args.workspace or not args.out_dir:
        ap.error("--workspace and --out-dir are required")
    # SystemExit unwinds through run_profile's finally, so a killed bench still tears down its driver.
    signal.signal(signal.SIGTERM, lambda signum, frame: sys.exit(128 + signum))

    bin_path = os.path.realpath(args.bin)
    if not os.access(bin_path, os.X_OK):
        print(f"binary not executable: {bin_path}", file=sys.stderr)
        return 1
    dataset = os.path.realpath(args.workspace)
    repo = os.path.join(dataset, args.repo)
    out_dir = os.path.abspath(args.out_dir)
    if os.path.exists(out_dir) and os.listdir(out_dir):
        print(f"refusing to write into non-empty {out_dir}", file=sys.stderr)
        return 1
    try:
        receipt = load_build_receipt(bin_path, args.build_receipt)
    except (OSError, ValueError) as e:
        print(f"build receipt rejected: {e}", file=sys.stderr)
        return 1
    os.makedirs(out_dir, exist_ok=True)

    report: dict[str, Any] = {
        "timestampUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "metadata": collect_metadata(
            bin_path,
            dataset,
            args.build_command,
            args.build_profile,
            build_receipt=receipt,
        ),
        "runsPerProfile": args.runs,
        "steadySeconds": args.steady_seconds,
        "sampleIntervalSec": args.sample_interval,
        "phaseDefinitions": PHASE_DEFINITIONS,
        "profiles": {},
    }
    failed = False
    profiles = ["idle", "1repo"] if args.profile == "all" else [args.profile]
    for profile in profiles:
        runs = []
        for i in range(1, args.runs + 1):
            run_dir = os.path.join(out_dir, profile, f"run-{i:02d}")
            print(f"[{profile} {i}/{args.runs}] {run_dir}", flush=True)
            r = run_profile(profile, bin_path, repo, run_dir, args.steady_seconds, args.sample_interval)
            if r["status"] == "COMPLETED":
                m = r["measurement"]["steadyMetrics"]
                print(f"  steady RSS {m['rssMedianMib']} MiB, PSS {m['pssMedianMib']} MiB", flush=True)
            else:
                failed = True
                print(f"  FAILED: {r['error']}", file=sys.stderr, flush=True)
            runs.append(r)
        prof_failed = any(run.get("status") != "COMPLETED" for run in runs)
        report["profiles"][profile] = {
            "status": "FAILED" if prof_failed else "COMPLETED",
            "summary": summarize(runs),
            "runs": runs,
        }
    report["profiles"]["15repo"] = {"status": "UNSUPPORTED", "reason": UNSUPPORTED_15REPO}
    report["status"] = "FAILED" if failed else "COMPLETED"

    write_report(out_dir, report)
    print(f"report: {out_dir}/tauri_baseline_report.md")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
