#!/usr/bin/env python3
"""
bench_native_memory.py - memory driver for the native GPUI workbench (Linux/X11 only).

Reuses memory_harness.py (attach mode); the shared helpers below own process identity,
isolation, teardown and summaries. Nothing here samples memory itself.

Per run it owns: a private Xvfb (display number from -displayfd, never guessed), a private
D-Bus with no service activation, fresh XDG dirs, and the app started under that bus. The app
PID is the unique descendant of our dbus-run-session whose exe is the binary under test.

Rendering uses Mesa lavapipe selected explicitly through VK_DRIVER_FILES (Xvfb has no DRI3).
Input is real X11 input through XTEST (`xdotool key`/`click` without --window, after
XSetInputFocus via `windowfocus`); no product call is made on the app's behalf.

Screenshots are taken from the root window and cropped to the app window's absolute
geometry; `xwd -id` of the window is captured alongside only to document how the two differ.

Readiness is not a startup log line: the app's own state lines (repo count, changed files,
history rows, previewed path) must match an independent `git` oracle, and the cropped
screenshot must be non-blank. See docs/native-perf-harness.md for what is and is not measured.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import math
import os
import re
import select
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from typing import Any, Callable

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


# ---------------------------------------------------------------- shared helpers
# Process identity, teardown, git oracle, attach-mode harness and run summaries,
# also used by check_native_leaks.py and run_native_acceptance.py.

ORACLE_MAX_BLOB_BYTES = 256 * 1024
MATCHED_REF = "refs/heads/feat/divergent"
MATCHED_SIZE = (1080, 720)
MATCHED_SENTINEL = b"snip-sync matched 1repo-diff: clipboard must stay unchanged\n"
MATCHED_SCENARIO = "selected two-commit feature branch on standard repository"


class BenchError(Exception):
    pass


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


def matched_oracle(repo: str) -> dict[str, Any]:
    """Fixed ref, dynamic OIDs/path: never fall back to the default history page."""
    history = git(repo, "log", "--topo-order", "--format=%H", MATCHED_REF, "--").splitlines()
    if len(history) != 2 or any(not re.fullmatch(r"[0-9a-f]{40}", oid) for oid in history):
        raise BenchError(f"{MATCHED_REF} must have exactly two full commit OIDs, got {history}")
    oracle = commit_oracle(repo, history[0])
    if oracle is None:
        raise BenchError(f"{MATCHED_REF} tip has no small text diff to verify")
    patch = git(repo, "diff", "--no-ext-diff", "--no-textconv", "--no-color",
                history[1], history[0], "--", f":(literal){oracle['path']}")
    if not patch or not changed_lines(patch):
        raise BenchError("matched tip diff is empty")
    in_hunk = False
    diff_rows = []
    for line in patch.splitlines():
        if line.startswith("@@"):
            in_hunk = True
        elif in_hunk and line[:1] in ("+", "-"):
            diff_rows.append({"kind": "change-addition" if line[0] == "+" else "change-deletion", "text": line[1:]})
    return {**oracle, "ref": MATCHED_REF, "historyOids": history, "patch": patch, "diffRows": diff_rows,
            "patchSha256": hashlib.sha256(patch.encode()).hexdigest(), "changedLines": changed_lines(patch)}


def check_matched_state(observed: dict[str, Any], oracle: dict[str, Any]) -> None:
    expected = {"ref": MATCHED_REF, "historyOids": oracle["historyOids"],
                "sha": oracle["sha"], "path": oracle["path"], "previewMode": "diff",
                "observedLocale": "en",
                "width": MATCHED_SIZE[0], "height": MATCHED_SIZE[1], "basketEmpty": True,
                "clipboardSha256": hashlib.sha256(MATCHED_SENTINEL).hexdigest()}
    for key, wanted in expected.items():
        if observed.get(key) != wanted:
            raise BenchError(f"matched {key}: observed {observed.get(key)!r}, expected {wanted!r}")


def check_matched_measurement(measurement: dict[str, Any]) -> None:
    if measurement.get("status") != "COMPLETED" or measurement.get("timestamps", {}).get("steadyDurationSec", 0) < 30:
        raise BenchError("matched measurement needs a completed 30-second steady window")
    metrics = measurement.get("steadyMetrics", {})
    for key in ("rssMedianMib", "rssP95Mib", "rssMaxMib", "pssMedianMib", "pssP95Mib", "pssMaxMib"):
        value = metrics.get(key)
        if not isinstance(value, (int, float)) or not 0 < value < float("inf"):
            raise BenchError(f"matched measurement lacks valid {key}: {value!r}")


def matched_identity(bin_path: str, repo: str) -> dict[str, Any]:
    """Hash the participating repository's refs/index/worktree and the dataset manifest."""
    manifest_path = os.path.join(os.path.dirname(repo), "workload_manifest.json")
    with open(manifest_path) as f:
        manifest = json.load(f)
    if manifest.get("preset") != "standard":
        raise BenchError("1repo-diff requires a standard workload manifest")
    if os.path.basename(repo) not in {entry["name"] for entry in manifest.get("repos", [])}:
        raise BenchError("participating repository is not listed in the standard manifest")
    digest = hashlib.sha256()
    for args in (("show-ref", "--head"), ("ls-files", "--stage", "-z"),
                 ("status", "--porcelain=v2", "-z", "--untracked-files=all")):
        digest.update(git(repo, *args, text=False))
        digest.update(b"\0")
    paths = set(git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard", text=False).split(b"\0"))
    for raw in sorted(paths - {b""}):
        path = os.path.join(os.fsencode(repo), raw)
        digest.update(raw + b"\0")
        if os.path.islink(path):
            value = b"link:" + os.readlink(path)
        elif os.path.isfile(path):
            value = sha256_file(path).encode()
        elif not os.path.exists(path):
            value = b"deleted"
        else:
            raise BenchError(f"unsupported workload entry {os.fsdecode(path)}")
        digest.update(value + b"\0")
    return {"binarySha256": sha256_file(bin_path), "manifestSha256": sha256_file(manifest_path),
            "repoPath": os.path.realpath(repo), "repoSha256": digest.hexdigest(),
            "scope": "participating repo refs, index, tracked/nonignored untracked worktree bytes; dataset manifest"}


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


# ---------------------------------------------------------------- native session

SCREEN = "1280x900x24"
ICD_DIR = "/usr/share/vulkan/icd.d"
# Same process waits for the sampler gate, then replaces itself. NativeSession writes this file.
PREEXEC_LAUNCHER = r"""import os, sys, time, json
ident_path, ready_path, bin_path = sys.argv[1], sys.argv[2], sys.argv[3]
app_args = sys.argv[4:]
pid = os.getpid()
try:
    with open(f'/proc/{pid}/stat') as f:
        starttime = int(f.read().rsplit(')', 1)[1].split()[19])
except Exception as e:
    sys.stderr.write(f'failed reading starttime: {e}\n')
    sys.exit(1)
tmp = ident_path + '.tmp'
with open(tmp, 'w') as f:
    json.dump({'pid': pid, 'starttime': starttime}, f)
os.replace(tmp, ident_path)
deadline = time.monotonic() + 45.0
while time.monotonic() < deadline:
    if os.path.exists(ready_path):
        break
    time.sleep(0.005)
else:
    sys.stderr.write('launcher timed out waiting for sampler_ready\n')
    sys.exit(1)
os.execv(bin_path, [bin_path] + app_args)
"""


def write_preexec_launcher(path: str) -> None:
    with open(path, "w", encoding="utf-8") as f:
        f.write(PREEXEC_LAUNCHER)
PROFILES = ("idle", "1repo", "1repo-diff", "15overview", "15active", "soak")
DEFAULT_PROFILES = tuple(profile for profile in PROFILES if profile != "1repo-diff")
# Built into WorkbenchModel::new of the D3 workbench (mov immediate 0x32). Not a CLI flag.
APPLICATION_HISTORY_PAGE_LENGTH = 50
DELETED_FILE_MARKER = b"// This file has been deleted in this change"
BOUNDS_RE = re.compile(r"\[APP:CTRL_BOUNDS: id=(?P<id>.+?) x=(?P<x>-?\d+) y=(?P<y>-?\d+) w=(?P<w>\d+) h=(?P<h>\d+)\]")
REPO_SELECT_RE = re.compile(r"\[APP:REPO_SELECTING: (?P<idx>\d+) \((?P<name>[^)]+)\)")
BASKET_RE = re.compile(r"\[APP:BASKET: n=(?P<n>\d+) summary=(?P<summary>.*)\]")
FILE_HEADER_RE = re.compile(
    rb"(?:^|\n)// file: (?:\[(?:NEW|MODIFIED|DELETED|MOVED)\] )*([^\r\n]+)\r?\n"
)
# Basket / preview labels for the three change groups the D3 list actually copies.
PREVIEW_KIND = {
    "staged": "staged_changes",
    "unstaged": "unstaged_changes",
    "untracked": "working_changes",
}


class NativeBenchError(Exception):
    pass


# ---------------------------------------------------------------- pure helpers (tested)


def e2e_scaled(seconds: float) -> float:
    """A deadline stretched by SNIP_E2E_TIMEOUT_SCALE, as snip_native_e2e::scaled does:
    lavapipe renders on the CPU, so a loaded machine makes a healthy run miss fixed waits.
    A missing, unparsable, non-finite or below-1 scale is 1."""
    try:
        scale = float(os.environ.get("SNIP_E2E_TIMEOUT_SCALE", "1"))
    except ValueError:
        scale = 1.0
    return seconds * (scale if math.isfinite(scale) and scale >= 1.0 else 1.0)


def lavapipe_icd(icd_dir: str = ICD_DIR, machine: str | None = None) -> str:
    """The lavapipe ICD manifest; file name differs between Mesa builds. Fails if absent."""
    machine = machine or os.uname().machine
    for name in ("lvp_icd.json", f"lvp_icd.{machine}.json"):
        path = os.path.join(icd_dir, name)
        if os.path.isfile(path):
            return path
    if os.path.isdir(icd_dir):
        for fname in sorted(os.listdir(icd_dir)):
            if fname.startswith("lvp_icd") and fname.endswith(".json"):
                return os.path.join(icd_dir, fname)
    raise NativeBenchError(f"no lavapipe ICD in {icd_dir} (install mesa-vulkan-drivers)")


def parse_bounds(lines: list[str]) -> dict[str, tuple[int, int, int, int]]:
    """Latest reported bounds per control id; a later CTRL_GONE removes it."""
    out: dict[str, tuple[int, int, int, int]] = {}
    for line in lines:
        m = BOUNDS_RE.search(line)
        if m:
            out[m["id"]] = (int(m["x"]), int(m["y"]), int(m["w"]), int(m["h"]))
            continue
        gone = re.search(r"\[APP:CTRL_GONE: id=(.+?)\]", line)
        if gone:
            out.pop(gone[1], None)
    return out


def parse_xwininfo(text: str) -> dict[str, Any]:
    """Absolute client geometry and map state from `xwininfo -id` output."""
    def field(label: str) -> str:
        m = re.search(rf"^\s*{re.escape(label)}:\s*(.+)$", text, re.M)
        if not m:
            raise NativeBenchError(f"xwininfo output lacks {label!r}")
        return m[1].strip()

    return {
        "x": int(field("Absolute upper-left X")),
        "y": int(field("Absolute upper-left Y")),
        "width": int(field("Width")),
        "height": int(field("Height")),
        "mapState": field("Map State"),
    }


def parse_repo_select(line: str) -> tuple[int, str]:
    """Index and repo name from `[APP:REPO_SELECTING: idx (name) root=...]`."""
    m = REPO_SELECT_RE.search(line)
    if not m:
        raise NativeBenchError(f"unparsed REPO_SELECTING line: {line}")
    return int(m["idx"]), m["name"]


def source_rows(repo: str) -> list[dict[str, Any]]:
    """Source-aware change rows, in the app's order.

    Matches `status_details`: porcelain v2, `--untracked-files=normal`, `--no-renames`.
    A path that is both staged and unstaged is two rows. The count is not the
    number of distinct paths.
    """
    raw = git(
        repo, "status", "--porcelain=v2", "-z",
        "--untracked-files=normal", "--no-renames", text=False,
    )
    staged: list[tuple[str, str]] = []
    unstaged: list[tuple[str, str]] = []
    untracked: list[str] = []
    conflicted: list[str] = []
    entries = raw.split(b"\0")
    i = 0
    while i < len(entries):
        rec = entries[i]
        i += 1
        if not rec:
            continue
        text = rec.decode("utf-8", "surrogateescape")
        if text.startswith("#") or text.startswith("!"):
            continue
        if text.startswith("? "):
            untracked.append(text[2:])
            continue
        if text.startswith("u "):
            parts = text[2:].split(" ", 9)
            if len(parts) < 10:
                raise NativeBenchError(f"unparsed conflict record: {text!r}")
            conflicted.append(parts[9])
            continue
        if text.startswith("1 "):
            parts = text[2:].split(" ", 7)
            if len(parts) < 8 or len(parts[0]) < 2:
                raise NativeBenchError(f"unparsed ordinary status record: {text!r}")
            x, y, path = parts[0][0], parts[0][1], parts[7]
            if x != ".":
                staged.append((path, x))
            if y != ".":
                unstaged.append((path, y))
            continue
        if text.startswith("2 "):
            parts = text[2:].split(" ", 8)
            if len(parts) < 9 or len(parts[0]) < 2:
                raise NativeBenchError(f"unparsed rename status record: {text!r}")
            x, y, path = parts[0][0], parts[0][1], parts[8]
            if x != ".":
                staged.append((path, x))
            if y != ".":
                unstaged.append((path, y))
            if i < len(entries):
                i += 1  # original path is the next NUL field
            continue
        raise NativeBenchError(f"unparsed status record: {text!r}")

    rows: list[dict[str, Any]] = []
    for path, xy in staged:
        rows.append({"path": path, "source": "staged", "deleted": xy == "D", "conflict": False})
    for path, xy in unstaged:
        rows.append({"path": path, "source": "unstaged", "deleted": xy == "D", "conflict": False})
    for path in untracked:
        rows.append({"path": path, "source": "untracked", "deleted": False, "conflict": False})
    for path in conflicted:
        rows.append({"path": path, "source": "conflicted", "deleted": False, "conflict": True})
    rows.sort(key=lambda row: row["path"])
    return rows


def history_page(repo: str, page_len: int = APPLICATION_HISTORY_PAGE_LENGTH) -> dict[str, Any]:
    """First history page the way `browser::history_with` asks git for it."""
    raw = git(
        repo, "log", "--topo-order", "--ignore-missing", f"-n{page_len + 1}",
        "--format=%H%x00%s", "--branches", "--remotes", "--tags", "HEAD", "--",
    )
    lines = [line for line in raw.splitlines() if line]
    if len(lines) > page_len:
        page = lines[:page_len]
        expected = page_len
    else:
        page = lines
        expected = len(page)
    tokens: list[str] = []
    first = None
    for line in page[:5]:
        sha, subject = line.split("\0", 1)
        short = sha[:7]
        if first is None:
            first = short
        tokens.extend((short, subject))
    if page and first is None:
        first = page[0].split("\0", 1)[0][:7]
    return {
        "historyRowsExpected": expected,
        "historyFirst": first,
        "newestCommits": tokens,
        "commitsReachable": int(git(repo, "rev-list", "--all", "--count")),
    }


def repo_oracle(repo: str) -> dict[str, Any]:
    rows = source_rows(repo)
    page = history_page(repo)
    return {
        "name": os.path.basename(repo.rstrip("/")),
        "repoPath": os.path.realpath(repo),
        "sourceRows": rows,
        "distinctPaths": len({row["path"] for row in rows}),
        **page,
    }


def source_file_bytes(repo: str, row: dict[str, Any]) -> bytes | None:
    """Bytes the basket export stores for this row, or None when they are not UTF-8 text.

    Staged content is the index blob. Unstaged and untracked content is the worktree
    file. A deletion is the core deleted-file marker, not the missing worktree bytes.
    """
    if row["deleted"]:
        return DELETED_FILE_MARKER
    if row["source"] == "staged":
        try:
            data = git(repo, "show", f":{row['path']}", text=False)
        except subprocess.CalledProcessError:
            return None
    else:
        disk = os.path.join(repo, row["path"])
        if not os.path.isfile(disk):
            return None
        try:
            with open(disk, "rb") as f:
                data = f.read()
        except OSError:
            return None
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        return None
    return data


def oracle_kind(row: dict[str, Any]) -> str:
    if row["deleted"]:
        return "deleted-marker"
    if row["source"] == "staged":
        return "index"
    return "worktree"


def choose_copy_target(repo: str, rows: list[dict[str, Any]]) -> tuple[dict[str, Any], bytes]:
    """One explicit row whose bytes identify the source.

    Prefer a path whose staged index bytes differ from the worktree (index A / working B).
    Otherwise the first non-deleted UTF-8 row in staged, unstaged, untracked order.
    """
    by_path: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        by_path.setdefault(row["path"], []).append(row)
    for path in sorted(by_path):
        group = by_path[path]
        staged = next((r for r in group if r["source"] == "staged" and not r["deleted"]), None)
        other = next((r for r in group if r["source"] in ("unstaged", "untracked") and not r["deleted"]), None)
        if staged is None or other is None:
            continue
        staged_bytes = source_file_bytes(repo, staged)
        other_bytes = source_file_bytes(repo, other)
        if staged_bytes is not None and other_bytes is not None and staged_bytes != other_bytes:
            return staged, staged_bytes
    for source in ("staged", "unstaged", "untracked"):
        for row in rows:
            if row["source"] != source or row["deleted"] or row["conflict"]:
                continue
            data = source_file_bytes(repo, row)
            if data is not None:
                return row, data
    raise NativeBenchError(
        f"no UTF-8 staged, unstaged, or untracked row to copy among {len(rows)} source rows"
    )


def clipcode_paths(payload: bytes) -> list[str]:
    """File paths declared by ClipCode `// file:` headers, in payload order."""
    try:
        payload.decode("utf-8")
    except UnicodeDecodeError as e:
        raise NativeBenchError(f"clipboard payload is not valid UTF-8: {e}") from e
    return [m.group(1).decode("utf-8") for m in FILE_HEADER_RE.finditer(payload)]


def assert_copied_payload(
    clip_bytes: bytes,
    *,
    root: str,
    path: str,
    expected: bytes,
    copied_count: int,
    sentinel: bytes | None = None,
) -> dict[str, Any]:
    """Byte oracle for one explicitly selected basket entry."""
    if sentinel is not None and (clip_bytes == sentinel or sentinel in clip_bytes):
        raise NativeBenchError("copy left the clipboard sentinel in place")
    if not clip_bytes:
        raise NativeBenchError("X11 clipboard is empty after COPY_DONE")
    try:
        clip_bytes.decode("utf-8")
    except UnicodeDecodeError as e:
        raise NativeBenchError(f"clipboard payload is not valid UTF-8: {e}") from e
    try:
        expected.decode("utf-8")
    except UnicodeDecodeError as e:
        raise NativeBenchError(f"oracle bytes are not valid UTF-8: {e}") from e

    expected_root = f"// clipcode-root: {root}"
    first_line = clip_bytes.split(b"\n", 1)[0].rstrip(b"\r")
    if first_line != expected_root.encode("utf-8"):
        got = first_line.decode("utf-8", errors="replace")
        raise NativeBenchError(f"clipboard lacks expected root header {expected_root!r}, got {got!r}")

    paths = clipcode_paths(clip_bytes)
    if paths != [path]:
        raise NativeBenchError(
            f"clipboard entries {paths} are not exactly the selected path [{path!r}]"
        )
    if copied_count != 1:
        raise NativeBenchError(f"COPY_DONE copied={copied_count}, expected 1 selected entry")

    extracted = extract_clipcode_file_bytes(clip_bytes, path, expected_root=root)
    if extracted is None:
        raise NativeBenchError(f"clipboard payload lacks entry for selected path {path!r}")
    if extracted != expected:
        raise NativeBenchError(
            f"copied bytes for {path!r} do not match the source oracle: "
            f"extracted {len(extracted)} bytes (sha256={hashlib.sha256(extracted).hexdigest()}), "
            f"oracle {len(expected)} bytes (sha256={hashlib.sha256(expected).hexdigest()})"
        )
    return {
        "verified": True,
        "copiedCount": copied_count,
        "paths": paths,
        "rootHeader": expected_root,
        "clipboardBytes": len(clip_bytes),
        "extractedBytes": len(extracted),
        "extractedSha256": hashlib.sha256(extracted).hexdigest(),
        "oracleBytes": len(expected),
        "oracleSha256": hashlib.sha256(expected).hexdigest(),
    }


def basket_events(lines: list[str]) -> list[dict[str, Any]]:
    events = []
    for line in lines:
        m = BASKET_RE.search(line)
        if not m:
            continue
        summary = m["summary"]
        entries = []
        if summary:
            for part in summary.split("; "):
                bits = part.split(" ", 2)
                if len(bits) != 3:
                    raise NativeBenchError(f"unparsed basket summary {summary!r}")
                entries.append({"repo": bits[0], "source": bits[1], "path": bits[2]})
        events.append({"n": int(m["n"]), "summary": summary, "entries": entries, "line": line})
    return events


def assert_basket_empty(lines: list[str], what: str) -> dict[str, Any]:
    """Any earlier non-empty basket event or selected toggle fails. Fresh slices use this."""
    events = basket_events(lines)
    bad = [event for event in events if event["n"] != 0]
    toggles = [line for line in lines if "[APP:FILE_TOGGLED:" in line and "selected=true" in line]
    if bad or toggles:
        detail = bad[-1]["line"] if bad else toggles[-1]
        raise NativeBenchError(f"{what} put entries in the basket: {detail}")
    return {"events": len(events), "nonEmpty": 0, "empty": True}


def assert_current_basket_empty(lines: list[str], what: str) -> dict[str, Any]:
    """Precondition: the latest basket event is empty. An earlier n=1 that was cleared does not count."""
    events = basket_events(lines)
    current = events[-1] if events else None
    if current is not None and current["n"] != 0:
        raise NativeBenchError(f"{what} put entries in the basket: {current['line']}")
    return {
        "events": len(events),
        "currentN": None if current is None else current["n"],
        "empty": True,
    }


def extract_clipcode_file_bytes(
    payload: bytes,
    path: str,
    *,
    expected_root: str | None = None,
    add_extra_line_between_files: bool = True,
) -> bytes | None:
    r"""Extracts exact raw unescaped file bytes for `path` from a ClipCode clipboard payload.

    Respects the ClipCode wire contract (default header '// file: $FILE_PATH'):
    - Strict UTF-8 validation (raises NativeBenchError on decode error; never lossy).
    - Anchors root header '// clipcode-root: <name>' if expected_root is provided.
    - Anchors file header line '^// file: (?:\[[A-Z]+\] )?<path>$' (with CRLF or LF).
    - Preserves file's exact line endings (CRLF or LF), trailing blank lines, and non-ASCII / BOM.
    - Reverses ClipCode escaping: strips leading '//clipcode-esc: ' from escaped body lines.
    - Reverses trailing inter-file delimiter added by serializer.
    - Returns None if the file header for `path` is not present in payload.
    """
    try:
        payload.decode("utf-8")
    except UnicodeDecodeError as e:
        raise NativeBenchError(f"clipboard payload is not valid UTF-8: {e}") from e

    if expected_root is not None:
        first_line = payload.split(b"\n", 1)[0].rstrip(b"\r")
        exp = b"// clipcode-root: " + expected_root.encode("utf-8")
        if first_line != exp:
            first_line_str = first_line.decode("utf-8", errors="replace")
            raise NativeBenchError(
                f"clipboard payload root header mismatch: expected {exp.decode('utf-8')!r}, got {first_line_str!r}"
            )

    pattern = re.compile(
        rb"(?:^|\n)// file: (?:\[(?:NEW|MODIFIED|DELETED|MOVED)\] )*"
        + re.escape(path.encode("utf-8"))
        + rb"\r?\n"
    )
    m = pattern.search(payload)
    if not m:
        return None
    body_start = m.end()

    boundary_re = re.compile(rb"\n// file: |\n// clipcode-end(?:\r?\n|$)")
    next_m = boundary_re.search(payload, pos=body_start)
    if next_m:
        body_end = next_m.start()
        raw_body = payload[body_start:body_end]
        if add_extra_line_between_files and raw_body.endswith(b"\n"):
            raw_body = raw_body[:-1]
    else:
        # One between-files delimiter, and, when empty pre/post wrappers are on
        # (unstaged/untracked; not the staged/deleted fallback), one more newline
        # from the empty post-text. The file's own trailing newline stays.
        raw_body = payload[body_start:]
        if add_extra_line_between_files and raw_body.endswith(b"\n"):
            if _empty_text_wrappers(payload) and raw_body.endswith(b"\n\n"):
                raw_body = raw_body[:-1]
            raw_body = raw_body[:-1]

    lines = raw_body.split(b"\n")
    esc = b"//clipcode-esc: "
    unescaped = [l[len(esc):] if l.startswith(esc) else l for l in lines]
    return b"\n".join(unescaped)


def _empty_text_wrappers(payload: bytes) -> bool:
    """True when the root header is followed by a blank line, then a file header.

    Default settings write that blank line for empty pre-text only when the basket
    is not staged/deleted fallback. The same mode appends an empty post-text newline
    after the last file and does not write `// clipcode-end`.
    """
    nl = payload.find(b"\n")
    if nl < 0 or not payload.startswith(b"// clipcode-root:"):
        return False
    return payload[nl + 1:].startswith(b"\n// file:")


def extract_clipcode_file_content(payload: str, path: str) -> str | None:
    """Compatibility text wrapper over extract_clipcode_file_bytes."""
    b = extract_clipcode_file_bytes(payload.encode("utf-8"), path)
    return b.decode("utf-8") if b is not None else None


def copy_explicit_selection(
    s: NativeSession,
    win: dict[str, Any],
    oracle: dict[str, Any],
    selection: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Select one source row with its real checkbox and copy it through btn-copy.

    The basket starts empty. Ctrl+C is not used: with the reader focused it copies
    the preview text instead of the basket. A path-only control id is not a substitute.
    `selection` picks one source row; otherwise the driver chooses a row whose bytes
    identify the source.
    """
    if selection is None:
        row, expected = choose_copy_target(oracle["repoPath"], oracle["sourceRows"])
    else:
        matches = [
            item for item in oracle["sourceRows"]
            if item["path"] == selection["path"] and item["source"] == selection["source"]
        ]
        if len(matches) != 1:
            raise NativeBenchError(
                f"selection {selection['source']} {selection['path']} is not one source row"
            )
        row = matches[0]
        expected = source_file_bytes(oracle["repoPath"], row)
        if expected is None:
            raise NativeBenchError(f"no UTF-8 oracle bytes for {row['source']} {row['path']}")
    path, source = row["path"], row["source"]
    if source not in PREVIEW_KIND:
        raise NativeBenchError(f"refusing to copy unsupported source {source!r} for {path}")
    kind = PREVIEW_KIND[source]
    row_id = f"change-row:{source}:{path}"
    chk_id = f"change-chk:{source}:{path}"
    show_changes(s, win)

    expand_change_dirs(s, win, row_id, oracle["name"])
    row_bounds = scroll_into_view(s, win, row_id)
    assert_on_window(row_bounds, win, row_id)
    before = len(s.lines)
    s.click(win, row_bounds)
    try:
        s.wait_line(lambda line: f"[APP:PREVIEW_LOADED: {path}]" in line, start=before, timeout=15)
        s.wait_line(
            lambda line: f"[APP:E2E_PREVIEW: source={kind} " in line and f"path={path} " in line,
            start=before, timeout=15,
        )
    except NativeBenchError as e:
        raise NativeBenchError(f"clicking {row_id} did not preview {kind} {path}: {e}") from e

    chk_bounds = scroll_into_view(s, win, chk_id)
    assert_on_window(chk_bounds, win, chk_id)
    before = len(s.lines)
    s.click(win, chk_bounds)
    try:
        _, _, basket_line = s.wait_line(lambda line: "[APP:BASKET: n=" in line, start=before, timeout=10)
    except NativeBenchError as e:
        raise NativeBenchError(f"clicking {chk_id} did not log a basket change: {e}") from e
    event = basket_events([basket_line])[0]
    wanted = [{"repo": oracle["name"], "source": source, "path": path}]
    if event["n"] != 1 or event["entries"] != wanted:
        raise NativeBenchError(
            f"basket after {chk_id} is n={event['n']} {event['entries']}, expected {wanted}"
        )

    copy_bounds = require_control(s.texts(), "btn-copy")
    assert_on_window(copy_bounds, win, "btn-copy")
    sentinel = f"SNIP-DRIVER-SENTINEL-{uuid.uuid4().hex}\n".encode()
    s.set_clipboard(sentinel)
    stuck = s.read_clipboard()
    if stuck != sentinel:
        raise NativeBenchError("clipboard sentinel did not stick before copy")

    before = len(s.lines)
    # The copy button enables on the frame after the basket update and drops
    # clicks until then. Re-click only while no copy log has appeared at all.
    copy_logs = ("[APP:COPY_PREP:", "[APP:COPY_REFUSED:", "[APP:COPY_BUSY]", "[APP:COPY_DONE:")
    for _ in range(5):
        s.click(win, copy_bounds)
        try:
            s.wait_line(lambda line: any(tag in line for tag in copy_logs), start=before, timeout=1.5)
            break
        except NativeBenchError:
            continue
    try:
        _, _, copy_line = s.wait_line(lambda line: "[APP:COPY_DONE:" in line, start=before, timeout=8)
    except NativeBenchError as e:
        refused = [line for line in s.texts(before) if "COPY_REFUSED" in line or "COPY_DONE" in line]
        raise NativeBenchError(f"btn-copy did not log COPY_DONE ({refused or 'no copy log'}): {e}") from e
    copied_m = re.search(r"copied=(\d+)", copy_line)
    if not copied_m:
        raise NativeBenchError(f"COPY_DONE line has no copied count: {copy_line}")
    copied_n = int(copied_m[1])

    deadline = time.monotonic() + 10.0
    clip_bytes = b""
    while True:
        try:
            clip_bytes = s.read_clipboard()
        except NativeBenchError:
            clip_bytes = b""
        if clip_bytes and sentinel not in clip_bytes:
            break
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    clip_path = os.path.join(s.run_dir, "clipboard.bin")
    with open(clip_path, "wb") as f:
        f.write(clip_bytes)
    checked = assert_copied_payload(
        clip_bytes, root=oracle["name"], path=path, expected=expected,
        copied_count=copied_n, sentinel=sentinel,
    )
    worktree_differs = None
    disk = os.path.join(oracle["repoPath"], path)
    if source == "staged" and not row["deleted"] and os.path.isfile(disk):
        with open(disk, "rb") as f:
            worktree = f.read()
        worktree_differs = worktree != expected
    return {
        "supported": True,
        "path": path,
        "source": source,
        "previewSource": kind,
        "oracleKind": oracle_kind(row),
        "worktreeDiffers": worktree_differs,
        "sentinelReplaced": True,
        "controls": {"row": row_id, "checkbox": chk_id, "copy": "btn-copy"},
        "basket": {"n": event["n"], "summary": event["summary"], "entries": event["entries"]},
        **checked,
    }


def workspace_repos(workspace: str) -> list[str]:
    """Repos the workspace holds, per core's rule: the root if it is one, plus direct children with .git."""
    repos = []
    if os.path.exists(os.path.join(workspace, ".git")):
        repos.append(workspace)
    if os.path.isdir(workspace):
        repos += sorted(
            os.path.join(workspace, d) for d in os.listdir(workspace)
            if os.path.exists(os.path.join(workspace, d, ".git"))
        )
    return repos


def check_repo_state(lines: list[str], oracle: dict[str, Any]) -> dict[str, Any]:
    """REPO_LOADED file count is source rows, not distinct paths. History is one built-in page."""
    name = oracle["name"]
    loaded = [line for line in lines if f"[APP:REPO_LOADED: {name} files=" in line]
    if not loaded:
        raise NativeBenchError(f"app never reported {name} loaded")
    files = int(re.search(r"files=(\d+)", loaded[-1])[1])
    rows = oracle["sourceRows"]
    distinct = len({row["path"] for row in rows})
    if files != len(rows):
        raise NativeBenchError(
            f"{name}: app lists {files} source rows, git status has {len(rows)} "
            f"source rows across {distinct} distinct paths"
        )
    graphs = [int(re.search(r"commits=(\d+)", line)[1]) for line in lines if "[APP:GRAPH_LOADED:" in line]
    expected_rows = oracle["historyRowsExpected"]
    if not graphs or graphs[-1] != expected_rows:
        raise NativeBenchError(
            f"{name}: history rows {graphs[-1:] or 'none'}, expected {expected_rows} "
            f"(application page length {APPLICATION_HISTORY_PAGE_LENGTH})"
        )
    first = oracle.get("historyFirst")
    if first:
        e2e = [line for line in lines if "[APP:E2E_LOG: mode=graph " in line]
        if not e2e:
            raise NativeBenchError(f"{name}: missing [APP:E2E_LOG] for the history page")
        got = re.search(r"first=(\S+)", e2e[-1])
        if not got or got[1] != first:
            raise NativeBenchError(
                f"{name}: history first {got[1] if got else 'missing'}, git topo-order page starts at {first}"
            )
    paths = {row["path"] for row in rows}
    previews = [
        line.split("[APP:PREVIEW_LOADED: ", 1)[1].rstrip("]")
        for line in lines if "[APP:PREVIEW_LOADED: " in line
    ]
    if rows:
        if not previews or previews[-1] not in paths:
            raise NativeBenchError(f"{name}: previewed {previews[-1:] or 'nothing'}, not a source-row path")
    elif previews:
        raise NativeBenchError(f"{name}: previewed {previews[-1]} but git status has no source rows")
    return {
        "changedFiles": files,
        "sourceRows": len(rows),
        "distinctPaths": distinct,
        "sourceIdentities": rows,
        "historyRows": graphs[-1],
        "historyFirst": first,
        "previewPath": previews[-1] if previews else None,
    }


def latency_stats(values_ms: list[float]) -> dict[str, Any] | None:
    if not values_ms:
        return None
    return {"n": len(values_ms), "medianMs": round(statistics.median(values_ms), 1),
            "p95Ms": round(calculate_p95(values_ms), 1), "maxMs": round(max(values_ms), 1)}


def tree_cpu_ticks(pids: list[int]) -> int:
    total = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as f:
                fields = f.read().rsplit(")", 1)[1].split()
            total += int(fields[11]) + int(fields[12])  # utime, stime
        except (OSError, IndexError, ValueError):
            pass
    return total


# ---------------------------------------------------------------- owned X session


def _pid_alive(pid: int) -> bool:
    """True while this pid still exists, including a zombie that has not been waited."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class NativeSession:
    """A private Xvfb + D-Bus + the app, all owned by this process."""

    def __init__(self, bin_path: str, workspace: str, mode: str, run_dir: str, e2e: bool):
        self.bin_path = os.path.realpath(bin_path)
        self.run_dir = run_dir
        self.iso_root: str | None = None
        self.xvfb_log = None
        self.xvfb = None
        self.proc = None
        self.reader = None
        self.log = None
        self._clip_proc = None
        self._pipe_fds: list[int] = []
        self.owned: list[dict[str, Any]] = []
        self.app = None
        self.lines: list[tuple[float, str]] = []
        self.env: dict[str, str] = {}
        try:
            self._open(workspace, mode, e2e)
        except BaseException:
            self.stop()
            raise

    def _close_fd(self, fd: int | None) -> None:
        if fd is None:
            return
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            self._pipe_fds.remove(fd)
        except ValueError:
            pass

    def _open(self, workspace: str, mode: str, e2e: bool) -> None:
        self.iso_root = tempfile.mkdtemp(prefix="snip-native-bench-")
        env, bus_config = isolated_env(self.iso_root)
        self.icd = lavapipe_icd()
        self.xvfb_log = open(os.path.join(self.run_dir, "xvfb.log"), "wb")
        read_fd, write_fd = os.pipe()
        self._pipe_fds.extend((read_fd, write_fd))
        self.xvfb = subprocess.Popen(
            ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", SCREEN, "-nolisten", "tcp"],
            pass_fds=(write_fd,), stdout=self.xvfb_log, stderr=self.xvfb_log, start_new_session=True,
        )
        self._close_fd(write_fd)
        self.owned.append(identity(self.xvfb.pid))
        # Under load Xvfb (xkbcomp, display-number probing) can take well over 15 s; giving up closes
        # the pipe and Xvfb then dies with "Cannot write display number to fd N".
        ready, _, _ = select.select([read_fd], [], [], e2e_scaled(60.0))
        display = os.read(read_fd, 64).decode().strip() if ready else ""
        self._close_fd(read_fd)
        if not display.isdigit():
            raise NativeBenchError(f"Xvfb did not report a display number (got {display!r})")
        env["DISPLAY"] = f":{display}"
        env.pop("WAYLAND_DISPLAY", None)
        env["VK_DRIVER_FILES"] = self.icd
        # Screenshot checks are calibrated on the dark palette.
        env["SNIP_THEME"] = "dark"
        env.pop("SNIP_NATIVE_E2E", None)
        if e2e:
            env["SNIP_NATIVE_E2E"] = "1"
        self.env = env
        # Pre-exec launcher gate: arranges sampler readiness before actual app execution.
        # The launcher runs inside dbus-run-session with fresh isolated XDG/bus env, writes its
        # (pid, starttime), waits for sampler_ready.signal from memory_harness, and then execv's
        # the target binary into the exact same PID and process tree without wrapper RAM overhead.
        self.ident_file = os.path.join(self.iso_root, "launcher_ident.json")
        self.sampler_ready_file = os.path.join(self.iso_root, "sampler_ready.signal")
        launcher_py = os.path.join(self.iso_root, "launcher.py")
        write_preexec_launcher(launcher_py)

        self.cmd = [
            "dbus-run-session", f"--config-file={bus_config}", "--",
            sys.executable, "-B", launcher_py, self.ident_file, self.sampler_ready_file,
            self.bin_path, "--workspace", workspace, "--mode", mode,
        ]
        self.isolation = {
            "display": env["DISPLAY"], "screen": SCREEN, "vkDriverFiles": self.icd,
            "xdgRoot": self.iso_root, "dbus": "private session bus, no service activation",
            "e2eInstrumentation": e2e,
            "samplerGatedExec": True,
        }
        self.log = open(os.path.join(self.run_dir, "app.log"), "w")
        self.proc = subprocess.Popen(
            self.cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, start_new_session=True,
        )
        self.reader = threading.Thread(target=self._read, name="native-app-log", daemon=True)
        self.reader.start()

    def _read(self) -> None:
        assert self.proc.stdout is not None
        for raw in self.proc.stdout:
            t = time.monotonic()
            text = raw.decode("utf-8", errors="replace").rstrip("\n")
            self.lines.append((t, text))
            self.log.write(f"{t:.4f} {text}\n")
            self.log.flush()

    def texts(self, start: int = 0) -> list[str]:
        return [t for _, t in self.lines[start:]]

    def wait_line(self, pred: Callable[[str], bool], start: int = 0, timeout: float = 60.0) -> tuple[int, float, str]:
        end = time.monotonic() + timeout
        i = start
        while time.monotonic() < end:
            while i < len(self.lines):
                t, text = self.lines[i]
                if pred(text):
                    return i, t, text
                i += 1
            if self.proc.poll() is not None and i >= len(self.lines):
                raise NativeBenchError(f"app exited with {self.proc.returncode} while waiting")
            time.sleep(0.01)
        raise NativeBenchError(f"timed out after {timeout}s waiting for app log line")

    def wait_app(self, on_app: Callable[[dict[str, Any]], None], timeout: float = 30.0) -> dict[str, Any]:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if hasattr(self, "ident_file") and os.path.exists(self.ident_file):
                try:
                    with open(self.ident_file) as f:
                        data = json.load(f)
                    pid, start = data["pid"], data["starttime"]
                    if read_proc_starttime(pid) == start:
                        ident = identity(pid)
                        ident["ageAtDiscoverySec"] = 0.0
                        ident["discoveryMonotonic"] = time.monotonic()
                        self.app = ident
                        on_app(ident)
                        self.remember_owned()
                        return ident
                except (json.JSONDecodeError, OSError):
                    pass
            pid = find_owned_app_pid(self.proc.pid, self.bin_path)
            if pid is not None:
                ident = identity(pid)
                if ident["starttime"] is not None:
                    ident["ageAtDiscoverySec"] = process_age_sec(pid)
                    ident["discoveryMonotonic"] = time.monotonic()
                    self.app = ident
                    on_app(ident)
                    self.remember_owned()
                    return ident
            if self.proc.poll() is not None:
                raise NativeBenchError(f"dbus-run-session exited with {self.proc.returncode} before the app appeared")
            time.sleep(0.005)
        raise NativeBenchError(f"no owned process running {self.bin_path} within {timeout}s")

    def assert_app_alive(self) -> None:
        if self.app is None or read_proc_starttime(self.app["pid"]) != self.app["starttime"]:
            raise NativeBenchError(f"app {self.app} exited or its PID was reused")

    def app_tree_pids(self) -> list[int]:
        if self.app is None:
            return []
        return [self.app["pid"], *descendants(self.app["pid"])]

    def remember_owned(self) -> None:
        if self.proc is None:
            return
        known = {(i["pid"], i["starttime"]) for i in self.owned}
        for p in [self.proc.pid, *descendants(self.proc.pid)]:
            ident = identity(p)
            if ident["starttime"] is not None and (ident["pid"], ident["starttime"]) not in known:
                self.owned.append(ident)

    # ---- X11

    def x(self, *args: str, timeout: float = 20.0) -> str:
        res = subprocess.run(list(args), env=self.env, capture_output=True, text=True, timeout=timeout)
        if res.returncode != 0:
            raise NativeBenchError(f"{' '.join(args)} exited {res.returncode}: {res.stderr.strip()[:300]}")
        return res.stdout

    def x_bytes(self, *args: str, timeout: float = 20.0) -> bytes:
        res = subprocess.run(list(args), env=self.env, capture_output=True, timeout=timeout)
        if res.returncode != 0:
            err = res.stderr.decode("utf-8", errors="replace").strip()[:300]
            raise NativeBenchError(f"{' '.join(args)} exited {res.returncode}: {err}")
        return res.stdout

    def set_clipboard(self, payload: bytes) -> None:
        """Own the clipboard with one foreground xclip process.

        `-silent` (the default) forks and the parent exits 0 while a daemon keeps
        the selection. `-quiet` stays in the foreground, so this Popen pid is the
        owner until another client replaces the selection or we signal that pid.
        """
        proc = subprocess.Popen(
            ["xclip", "-selection", "clipboard", "-i", "-quiet"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=self.env,
            start_new_session=True,
        )
        assert proc.stdin is not None
        try:
            proc.stdin.write(payload)
            proc.stdin.close()
        except Exception:
            proc.kill()
            proc.wait(timeout=2)
            raise
        previous = self._clip_proc
        self._clip_proc = proc
        if previous is not None:
            self._release_clip(previous, kill=False)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise NativeBenchError(
                    f"xclip -quiet pid {proc.pid} exited {proc.returncode} before the clipboard could be read"
                )
            try:
                if self.read_clipboard(timeout=1) == payload:
                    return
            except (NativeBenchError, subprocess.TimeoutExpired):
                pass
            time.sleep(0.05)
        raise NativeBenchError("clipboard sentinel was not readable within 5s")

    def _release_clip(self, proc: subprocess.Popen, *, kill: bool) -> None:
        """Reap this exact xclip pid. `kill` is for teardown; a handoff waits first.

        Exit status 0 is not treated as proof the owner is gone until `wait` has
        reaped this pid. No other xclip process is signalled.
        """
        if kill and proc.poll() is None:
            proc.kill()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            if proc.poll() is None:
                proc.kill()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                raise NativeBenchError(f"xclip clipboard owner pid {proc.pid} did not exit") from None
        if _pid_alive(proc.pid):
            raise NativeBenchError(f"xclip clipboard owner pid {proc.pid} still alive after wait")

    def read_clipboard(self, timeout: float = 20.0) -> bytes:
        return self.x_bytes("xclip", "-selection", "clipboard", "-o", timeout=timeout)

    def window(self, timeout: float = 20.0) -> dict[str, Any]:
        assert self.app is not None
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            res = subprocess.run(["xdotool", "search", "--pid", str(self.app["pid"])], env=self.env, capture_output=True, text=True)
            for wid in res.stdout.split():
                geo = parse_xwininfo(self.x("xwininfo", "-id", wid))
                if geo["mapState"] == "IsViewable" and geo["width"] > 1:
                    return {"wid": wid, **geo}
            time.sleep(0.1)
        raise NativeBenchError("no viewable window for the app PID")

    def focus(self, wid: str) -> None:
        # No window manager on Xvfb, so `windowactivate` is unsupported; set X input focus directly.
        self.x("xdotool", "windowfocus", "--sync", wid)
        focused = self.x("xdotool", "getwindowfocus").strip()
        if focused != wid:
            raise NativeBenchError(f"X focus is {focused}, not the app window {wid}")

    def key(self, wid: str, keys: str) -> float:
        """Real XTEST key press on the focused window; returns the monotonic send time."""
        self.focus(wid)
        time.sleep(0.05)
        t = time.monotonic()
        self.x("xdotool", "key", keys)
        self.x("xdotool", "keyup", "ctrl", "alt", "shift", "super")
        return t

    def click(self, win: dict[str, Any], bounds: tuple[int, int, int, int]) -> float:
        x, y, w, h = bounds
        ax, ay = win["x"] + x + w // 2, win["y"] + y + h // 2
        self.focus(win["wid"])
        time.sleep(0.05)
        t = time.monotonic()
        # No --sync: xdotool waits for a motion event, which never comes when the
        # pointer is already on this pixel. Move and click are one invocation.
        self.x("xdotool", "mousemove", str(ax), str(ay), "click", "1")
        return t

    def capture(self, win: dict[str, Any], name: str, timeout: float = 20.0) -> dict[str, Any]:
        """Root screenshot cropped to the window's absolute geometry, plus `xwd -id` for comparison.

        Both are retried every 0.5 s until the root crop is non-blank or timeout; the attempt log
        records when each path first showed content, which is the evidence for the capture method.
        """
        root_xwd = os.path.join(self.iso_root, f"{name}-root.xwd")
        win_xwd = os.path.join(self.iso_root, f"{name}-window.xwd")
        png = os.path.join(self.run_dir, f"{name}.png")
        win_png = os.path.join(self.run_dir, f"{name}-xwd-id.png")
        # On this Xvfb (no WM) the app presents no frame until it holds X input focus; mapping
        # alone is not enough (docs/native-perf-harness.md, first-paint probe).
        self.focus(win["wid"])
        start = time.monotonic()
        attempts = []
        while True:
            self.x("xwd", "-root", "-silent", "-out", root_xwd)
            self.x("convert", root_xwd, "-crop", f"{win['width']}x{win['height']}+{win['x']}+{win['y']}", "+repage", png)
            self.x("xwd", "-id", win["wid"], "-silent", "-out", win_xwd)
            self.x("convert", win_xwd, win_png)
            stats = {"rootCrop": image_stats(self, png), "xwdId": image_stats(self, win_png)}
            attempts.append({"afterFocusSec": round(time.monotonic() - start, 2), **{k: v["stddev"] for k, v in stats.items()}})
            if stats["rootCrop"]["stddev"] >= 0.02 or time.monotonic() - start > timeout:
                break
            time.sleep(0.5)
        if stats["rootCrop"]["stddev"] < 0.02:
            raise NativeBenchError(f"root-cropped screenshot {png} still blank after {timeout}s: {attempts}")
        return {"rootCrop": png, "rootCropStats": stats["rootCrop"], "xwdId": win_png, "xwdIdStats": stats["xwdId"],
                "attempts": attempts, "geometry": {k: win[k] for k in ("x", "y", "width", "height")}}

    def _close_handle(self, attr: str) -> None:
        handle = getattr(self, attr, None)
        if handle is None or getattr(handle, "closed", True):
            return
        handle.close()

    def _close_stdout(self) -> None:
        proc = self.proc
        if proc is None:
            return
        stdout = getattr(proc, "stdout", None)
        if stdout is not None and not stdout.closed:
            stdout.close()

    def stop(self, reader_join_timeout: float = 2.0) -> list[str]:
        """Release every helper this session started. Safe to call more than once.

        The clipboard owner is reaped by its own pid before Xvfb is killed. Closing
        the display is not what proves that xclip exited. The app-log file stays
        open while its reader thread is still alive.
        """
        problems: list[str] = []
        for fd in list(getattr(self, "_pipe_fds", ())):
            self._close_fd(fd)
        clip = getattr(self, "_clip_proc", None)
        self._clip_proc = None
        if clip is not None:
            try:
                self._release_clip(clip, kill=True)
            except NativeBenchError as e:
                problems.append(str(e))
        try:
            self.remember_owned()
        except Exception as e:  # noqa: BLE001
            problems.append(f"remember owned processes: {e}")
        for proc in (self.proc, self.xvfb):
            if proc is None:
                continue
            try:
                cleanup_process_group(os.getpgid(proc.pid), proc)
            except ProcessLookupError:
                pass
            except Exception as e:  # noqa: BLE001
                problems.append(f"process group of {proc.args[0]}: {e}")
        problems += reap_owned(self.owned)
        reader = self.reader
        reader_alive = False
        if reader is not None and reader.ident is not None:
            reader.join(timeout=reader_join_timeout)
            reader_alive = reader.is_alive()
            if reader_alive:
                problems.append("app log reader did not stop")
        if not reader_alive:
            self._close_stdout()
            self._close_handle("log")
        self._close_handle("xvfb_log")
        if self.iso_root and os.path.isdir(self.iso_root):
            shutil.rmtree(self.iso_root, ignore_errors=True)
        return problems


def image_stats(s: NativeSession, png: str) -> dict[str, Any]:
    out = s.x("convert", png, "-colorspace", "Gray", "-format", "%[fx:mean] %[fx:standard_deviation] %w %h", "info:").split()
    return {"mean": round(float(out[0]), 4), "stddev": round(float(out[1]), 4), "width": int(out[2]), "height": int(out[3]),
            "bytes": os.path.getsize(png)}


# ---------------------------------------------------------------- profiles


def check_native_matched(lines: list[str], win: dict[str, Any], oracle: dict[str, Any],
                         clipboard: bytes) -> dict[str, Any]:
    """Actual live row bounds give display order; full preview OID and hash bind the diff."""
    refs = [line for line in lines if "[APP:REF_FILTER: " in line]
    locales = [line for line in lines if "[APP:LOCALE: " in line]
    if not locales or locales[-1] != "[APP:LOCALE: En]":
        raise NativeBenchError("matched native locale must be observed English")
    graphs = [line for line in lines if "[APP:GRAPH_LOADED: " in line]
    logs = [line for line in lines if "[APP:E2E_LOG: " in line]
    if not graphs or graphs[-1] != "[APP:GRAPH_LOADED: commits=2]":
        raise NativeBenchError("matched graph must report exactly two commits")
    expected_log = f"[APP:E2E_LOG: mode=graph n=2 first={oracle['sha'][:7]} page=1]"
    if not logs or logs[-1] != expected_log:
        raise NativeBenchError(f"matched history log mismatch: {logs[-1:]}")
    rows = sorted(((key.removeprefix("commit-row:"), box) for key, box in parse_bounds(lines).items()
                   if key.startswith("commit-row:")), key=lambda item: item[1][1])
    short_oids = [oid[:7] for oid in oracle["historyOids"]]
    if len(set(short_oids)) != 2 or [short for short, _ in rows] != short_oids:
        raise NativeBenchError(f"matched visible history OID order differs: {[short for short, _ in rows]}")
    for short, box in rows:
        assert_on_window(box, win, f"commit-row:{short}")
    previews = [line for line in lines if "[APP:E2E_PREVIEW: " in line]
    preview = re.fullmatch(r"\[APP:E2E_PREVIEW: source=(\S+) rev=(\S+) path=(.+) lines=(\d+) fnv=([0-9a-f]+)\]",
                           previews[-1] if previews else "")
    if preview is None:
        raise NativeBenchError("missing matched E2E_PREVIEW")
    fingerprint = 0xcbf29ce484222325
    for byte in oracle["patch"].encode():
        fingerprint = ((fingerprint ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    if preview[1] != "commit_diff" or int(preview[4]) != len(oracle["patch"].splitlines()) or int(preview[5], 16) != fingerprint:
        raise NativeBenchError("matched retained diff source/line count/fingerprint differs from Git patch")
    if any("COPY_DONE:" in line or "SELECTION_COPIED:" in line for line in lines):
        raise NativeBenchError("matched profile unexpectedly copied content")
    basket = assert_basket_empty(lines, "matched diff")
    observed = {"ref": refs[-1].removeprefix("[APP:REF_FILTER: ").removesuffix("]") if refs else None,
                "historyOids": oracle["historyOids"], "displayedHistoryShortOids": [short for short, _ in rows],
                "historyEvidence": "two unique Git short OIDs, in live row y-order; full OID on E2E_PREVIEW",
                "sha": preview[2], "path": preview[3], "previewMode": "diff", "width": win["width"],
                "observedLocale": "en",
                "height": win["height"], "basketEmpty": basket["empty"], "basket": basket,
                "clipboardSha256": hashlib.sha256(clipboard).hexdigest(),
                "retainedPatchMatchedGit": True, "patchSha256": oracle["patchSha256"]}
    check_matched_state(observed, oracle)
    return observed


def select_native_matched(s: NativeSession, win: dict[str, Any], oracle: dict[str, Any]) -> None:
    def click(control: str) -> None:
        deadline = time.monotonic() + 20
        previous = None
        stable_at = time.monotonic()
        while time.monotonic() < deadline:
            bounds = parse_bounds(s.texts()).get(control)
            if bounds != previous:
                previous, stable_at = bounds, time.monotonic()
            if bounds and time.monotonic() - stable_at >= 0.2:
                assert_on_window(bounds, win, control)
                s.click(win, bounds)
                return
            time.sleep(0.04)
        raise NativeBenchError(f"matched control {control} has no stable live bounds")

    if (win["width"], win["height"]) != MATCHED_SIZE:
        raise NativeBenchError(f"matched client geometry is {win['width']}x{win['height']}, expected {MATCHED_SIZE}")
    s.set_clipboard(MATCHED_SENTINEL)
    start = len(s.lines)
    # The frozen native app starts in zh-Hant; the matched profile runs in English via the real toggle.
    click("btn-locale")
    s.wait_line(lambda line: line == "[APP:LOCALE: En]", start=start)
    ref_control = f"ref:{MATCHED_REF}"
    bounds = parse_bounds(s.texts()).get(ref_control)
    if bounds and visible_in(bounds, (0, 0, win["width"], win["height"])) == 0:
        click(ref_control)
    else:
        click("btn-ref-selector")
        click("selector-input")
        s.key(win["wid"], "ctrl+a")
        s.x("xdotool", "type", "--clearmodifiers", MATCHED_REF.removeprefix("refs/heads/"))
        click(f"pick-ref:{MATCHED_REF}")
    s.wait_line(lambda line: line == f"[APP:REF_FILTER: {MATCHED_REF}]", start=start)
    s.wait_line(lambda line: line == "[APP:GRAPH_LOADED: commits=2]", start=start)
    s.wait_line(lambda line: line == f"[APP:E2E_LOG: mode=graph n=2 first={oracle['sha'][:7]} page=1]", start=start)
    click(f"commit-row:{oracle['sha'][:7]}")
    s.wait_line(lambda line: "[APP:E2E_PREVIEW: source=commit_diff " in line and f"rev={oracle['sha']} " in line, start=start)
    click(f"commit-file:{oracle['path']}")
    s.wait_line(lambda line: "[APP:E2E_PREVIEW: source=commit_diff " in line and f"rev={oracle['sha']} path={oracle['path']} " in line, start=start)


def drive(s: NativeSession, profile: str, workspace: str, run_dir: str, steady: float, interval: float,
          soak_switches: int, result: dict[str, Any], build_profile: str = "unknown") -> None:
    ready_file = os.path.join(run_dir, f"ready-{uuid.uuid4().hex}.signal")
    marker = f"[READY:NATIVE:{profile.upper()}:{uuid.uuid4().hex}]"
    harness: subprocess.Popen | None = None
    harness_log = open(os.path.join(run_dir, "harness.log"), "wb")
    profile_label = f"native {build_profile} {profile}"
    sampler_ready = getattr(s, "sampler_ready_file", None)
    try:
        def attach(app: dict[str, Any]) -> None:
            nonlocal harness
            harness = spawn_attached_harness(
                app, run_dir, ready_file, marker, steady, interval,
                profile_label, harness_log, readiness_timeout=900,
                sampler_ready_file=sampler_ready, expected_exe=s.bin_path,
            )

        result["app"] = s.wait_app(attach)
        s.wait_line(lambda l: "[APP:WINDOW_READY]" in l)
        repos = workspace_repos(workspace)
        _, _, line = s.wait_line(lambda l: "[APP:READY_REPOS:" in l, timeout=120)
        count = int(re.search(r"READY_REPOS: (\d+)", line)[1])
        if count != len(repos):
            raise NativeBenchError(f"app reports {count} repos, workspace holds {len(repos)}")
        state: dict[str, Any] = {"reposReported": count, "reposExpected": len(repos)}
        latencies: dict[str, list[float]] = {}
        selected_repo: str | None = None

        if repos:
            _, _, sel = s.wait_line(lambda l: "[APP:REPO_SELECTING:" in l)
            idx, name = parse_repo_select(sel)
            if idx != 0:
                raise NativeBenchError(f"startup selected index {idx} ({name}), expected repository 0")
            selected_repo = next(r for r in repos if os.path.basename(r) == name)
            wait_repo_loaded(s, selected_repo, 0)
            state["selected"] = check_repo_state(s.texts(), repo_oracle(selected_repo))
            state["basketAfterLoad"] = assert_basket_empty(s.texts(), f"loading {name}")

        win = s.window()
        result["window"] = win
        if profile == "15active":
            if len(repos) < 2:
                raise NativeBenchError("15active needs at least two repositories")
            target = repos[1]
            sent, t_loaded, t_graph, switched = click_repo(s, win, target)
            latencies = {
                "clickToRepoLoadedMs": [(t_loaded - sent) * 1000],
                "clickToGraphLoadedMs": [(t_graph - sent) * 1000],
            }
            state["switch"] = {
                "method": "repo-row click",
                "control": f"repo-row:{os.path.basename(target)}",
                "alt2Bound": False,
            }
            selected_repo = target
            state["selected"] = check_repo_state(s.texts(switched), repo_oracle(selected_repo))
        elif profile == "soak":
            latencies, state["soak"] = run_soak(s, win, repos, soak_switches)
            selected_repo = next(r for r in repos if os.path.basename(r) == state["soak"]["lastRepo"])
            state["selected"] = state["soak"].pop("lastState")

        # The screenshot shows the explicitly selected source, not whatever the app previewed on load.
        if profile == "1repo-diff":
            if selected_repo is None or len(repos) != 1:
                raise NativeBenchError("1repo-diff requires exactly one selected repository")
            oracle = matched_oracle(selected_repo)
            select_native_matched(s, win, oracle)
            # Wait for the two-row layout to retire the old page's probes.
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                live = parse_bounds(s.texts())
                if len([key for key in live if key.startswith("commit-row:")]) == 2:
                    break
                time.sleep(0.05)
            state["matched"] = check_native_matched(s.texts(), s.window(), oracle, s.read_clipboard())
            state["startupSelected"] = state.pop("selected")
            state["oracle"] = {key: value for key, value in oracle.items() if key not in ("content", "patch")}
            state["copyPerformed"] = False
            state["priorActions"] = ["startup auto-load and working preview", "toggle locale to English", "select ref", "select tip", "select file", "diff"]
            state["sourceContentMatchedGitShow"] = False
        elif selected_repo:
            oracle = repo_oracle(selected_repo)
            if not oracle["sourceRows"]:
                raise NativeBenchError(f"{oracle['name']} has no source rows to copy")
            state["basketBeforeCopy"] = assert_basket_empty(s.texts(), "before explicit copy")
            state["selected"]["autoPreviewPath"] = state["selected"].get("previewPath")
            copied = copy_explicit_selection(s, win, oracle)
            state["selected"]["clipboard"] = copied
            state["selected"]["previewPath"] = copied["path"]
            state["selected"]["previewSource"] = copied["previewSource"]
        else:
            state["clipboard"] = {"supported": False, "reason": f"{profile} has no selected repo"}

        s.assert_app_alive()
        shot = s.capture(s.window(), f"ready-{profile}")
        result["ui"] = {"state": state, "screenshot": shot,
                        "latency": {k: latency_stats(v) for k, v in latencies.items()}}
        s.remember_owned()
        result["appTreeAtReady"] = [identity(p) for p in s.app_tree_pids()]

        ticks0, t0 = tree_cpu_ticks(s.app_tree_pids()), time.monotonic()
        tmp = ready_file + ".tmp"
        with open(tmp, "w") as f:
            f.write(marker + "\n")
        os.replace(tmp, ready_file)
        assert harness is not None
        code = harness.wait(timeout=steady + 900)
        ticks1, t1 = tree_cpu_ticks(s.app_tree_pids()), time.monotonic()
        if code != 0:
            raise NativeBenchError(f"memory_harness exited {code}; see {run_dir}/harness.log")
        s.assert_app_alive()
        if profile == "1repo-diff":
            state["matchedAtEnd"] = check_native_matched(s.texts(), s.window(), oracle, s.read_clipboard())
        result["steadyCpuPercent"] = round(100 * (ticks1 - ticks0) / os.sysconf("SC_CLK_TCK") / (t1 - t0), 2)
        with open(os.path.join(run_dir, "benchmark_report.json")) as f:
            result["measurement"] = json.load(f)["results"][0]
        if profile == "1repo-diff":
            check_matched_measurement(result["measurement"])
        app = s.app
        assert app is not None
        result["processAgeAtSamplerStartSec"] = round(
            app["ageAtDiscoverySec"] + result["measurement"]["timestamps"]["startMonotonic"] - app["discoveryMonotonic"], 3)
        s.remember_owned()
        result["appTreeAtEnd"] = [identity(p) for p in s.app_tree_pids()]
        result["controllerAtEnd"] = sample_processes([s.xvfb.pid, s.proc.pid])
        result["graphicsLibsMapped"] = loaded_graphics_libs(app["pid"])
    finally:
        if harness is not None and harness.poll() is None:
            harness.kill()
            harness.wait()
        harness_log.close()
        for leftover in (ready_file, ready_file + ".tmp"):
            if os.path.exists(leftover):
                os.remove(leftover)


def wait_repo_loaded(s: NativeSession, repo: str, start: int, timeout: float = 120.0) -> tuple[float, float]:
    """Monotonic times of this repo's REPO_LOADED and the GRAPH_LOADED after it.

    Also waits for PREVIEW_LOADED when git says the repo has changes (the app previews the
    first one); preview and history load concurrently, so their order is not fixed.
    """
    name = os.path.basename(repo)
    i, t_loaded, _ = s.wait_line(lambda l: f"[APP:REPO_LOADED: {name} files=" in l, start=start, timeout=timeout)
    _, t_graph, _ = s.wait_line(lambda l: "[APP:GRAPH_LOADED:" in l, start=i, timeout=timeout)
    if source_rows(repo):
        s.wait_line(lambda l: "[APP:PREVIEW_LOADED: " in l, start=start, timeout=timeout)
    return t_loaded, t_graph


def visible_in(row: tuple[int, int, int, int], viewport: tuple[int, int, int, int]) -> int:
    """0 if row lies fully inside viewport vertically (with 2px tolerance), else the wheel direction to bring it in (+1 down, -1 up)."""
    if row[1] + 2 < viewport[1]:
        return -1
    if row[1] + row[3] - 2 > viewport[1] + viewport[3]:
        return 1
    return 0


def require_control(lines: list[str], control_id: str) -> tuple[int, int, int, int]:
    """Latest bounds for one control. A path-only id is not a stand-in for a source-aware id."""
    bounds = parse_bounds(lines)
    if control_id not in bounds:
        extra = ""
        head, sep, tail = control_id.partition(":")
        if sep and ":" in tail and head in ("change-row", "change-chk"):
            legacy = f"{head}:{tail.split(':', 1)[1]}"
            if legacy in bounds:
                extra = f"; {legacy} is visible and is not accepted"
        raise NativeBenchError(f"required control {control_id} has no [APP:CTRL_BOUNDS]{extra}")
    box = bounds[control_id]
    if box[2] <= 0 or box[3] <= 0:
        raise NativeBenchError(f"required control {control_id} has empty bounds {box}")
    return box


def assert_on_window(bounds: tuple[int, int, int, int], win: dict[str, Any], control_id: str) -> None:
    x, y, w, h = bounds
    if x < -2 or y < -2 or x + w > win["width"] + 2 or y + h > win["height"] + 2:
        raise NativeBenchError(
            f"{control_id} bounds {bounds} are outside the window {win['width']}x{win['height']}"
        )


def left_viewport(lines: list[str]) -> tuple[int, int, int, int]:
    """Visible area of the left list. `left-list` is that viewport; `left-scroll` is not accepted."""
    box = parse_bounds(lines).get("left-list")
    if box is None:
        raise NativeBenchError(
            "required control 'left-list' has no [APP:CTRL_BOUNDS]; refusing to use left-scroll"
        )
    return box


def _last_reported_bounds(lines: list[str], control: str) -> tuple[int, int, int, int] | None:
    """Last bounds in this repo/view, retained after a same-view CTRL_GONE.

    Repo rows belong to the workspace; selecting another repo does not retire
    their scroll hints. Path-only tree rows still belong to the selected repo.
    """
    found: tuple[int, int, int, int] | None = None
    for line in lines:
        if ("[APP:REPO_SELECTING:" in line and not control.startswith("repo-row:")) or (
            control.startswith(("rev-row:", "rev-chk:")) and "[APP:REV_TREE:" in line
        ):
            found = None
        match = BOUNDS_RE.search(line)
        if match and match["id"] == control:
            found = (int(match["x"]), int(match["y"]), int(match["w"]), int(match["h"]))
    return found


def scroll_into_view(
    s: NativeSession,
    win: dict[str, Any],
    control: str,
    max_steps: int = 60,
    settle_seconds: float = 0.2,
) -> tuple[int, int, int, int]:
    """Bounds of a control after it has stopped moving inside the left list.

    A repo click reflows the project tree. Clicking the first in-view sample
    hits whichever row has since moved onto that rectangle.
    """
    s.wait_line(lambda line: "id=left-list " in line, timeout=10)
    steps = 0
    last: tuple[int, int, int, int] | None = None
    stable_at: float | None = None
    deadline = time.monotonic() + 20
    waited = False
    while time.monotonic() < deadline and steps <= max_steps:
        # Taken before the read, so a line landing after it still counts as news.
        before = len(s.lines)
        lines = s.texts()
        viewport = left_viewport(lines)
        box = parse_bounds(lines).get(control)
        if box is None and not waited:
            # The row may just not be painted yet; give it a frame before scrolling away.
            waited = True
            try:
                s.wait_line(lambda line: f"id={control} " in line, start=before, timeout=0.5)
                continue
            except NativeBenchError:
                pass
        if box is not None and visible_in(box, viewport) == 0:
            if box == last and stable_at is not None and time.monotonic() - stable_at >= settle_seconds:
                return box
            if box != last:
                last = box
                stable_at = time.monotonic()
            time.sleep(0.04)
            continue
        if steps == max_steps:
            break
        last = None
        stable_at = None
        remembered = _last_reported_bounds(lines, control)
        if box is not None:
            direction = visible_in(box, viewport)
        elif remembered is not None and remembered[1] < viewport[1] + viewport[3] // 2:
            direction = -1  # the row left upward; wheel down would stay at the bottom
        else:
            direction = 1
        s.focus(win["wid"])
        cx = win["x"] + viewport[0] + viewport[2] // 2
        cy = win["y"] + viewport[1] + max(1, viewport[3] // 2)
        s.x("xdotool", "mousemove", str(cx), str(cy), "click", "5" if direction > 0 else "4")
        try:
            s.wait_line(lambda line: "id=left-list " in line or f"id={control} " in line, start=before, timeout=5)
        except NativeBenchError:
            pass  # a wheel event can be dropped; the next step sends another
        steps += 1
        time.sleep(0.03)
    raise NativeBenchError(f"required control {control} not settled inside left-list after {steps} wheel steps")


def change_row_group(control_id: str) -> str:
    """Changes group of a `change-row:<source>:<path>` row: untracked files list under Unstaged."""
    source = control_id.split(":", 2)[1]
    return "unstaged" if source == "untracked" else source


def _change_rows_snapshot(s: NativeSession) -> dict[str, tuple[int, int, int, int]]:
    return {k: v for k, v in parse_bounds(s.texts()).items() if k.startswith("change-")}


def _wheel_left_list(s: NativeSession, win: dict[str, Any], down: bool) -> bool:
    """One wheel step over the left list; False when the rows did not move."""
    snap = _change_rows_snapshot(s)
    viewport = left_viewport(s.texts())
    before = len(s.lines)
    s.focus(win["wid"])
    cx = win["x"] + viewport[0] + viewport[2] // 2
    cy = win["y"] + viewport[1] + max(1, viewport[3] // 2)
    s.x("xdotool", "mousemove", str(cx), str(cy), "click", "5" if down else "4")
    try:
        s.wait_line(lambda line: "id=change-" in line, start=before, timeout=1.5)
    except NativeBenchError:
        pass  # a wheel event can be dropped, or the list is already at its end
    time.sleep(0.1)
    return _change_rows_snapshot(s) != snap


def scroll_to_change_dirs(
    s: NativeSession,
    win: dict[str, Any],
    candidates: Callable[[], list[str] | None],
    max_steps: int = 60,
) -> list[str] | None:
    """Sweep the Changes list from its top down until `candidates` finds something.

    A workspace of many dirty repos puts a repo's folders out of view, above or
    below, and bounds of unpainted rows are unknown; a list that stops moving for
    two wheel steps is at its end.
    """
    for down in (False, True):
        still = 0
        for _ in range(max_steps):
            if down:
                found = candidates()
                if found != []:
                    return found
            if _wheel_left_list(s, win, down):
                still = 0
            else:
                still += 1
                if still >= 2:
                    break
    return candidates()


def expand_change_repo(s: NativeSession, win: dict[str, Any], control: str, repo: str, timeout: float = 10.0) -> bool:
    """Open the repo group above a Changes file row; False when nothing needed opening.

    A multi-repo workspace lists Changes as group > repo > directory, and repo groups start
    collapsed, so no `change-row` exists until `change-repo:<group>:<repo>` is clicked. A
    single-repo workspace has no repo rows and is left alone.
    """
    group = change_row_group(control)
    node = f"change-repo:{group}:{repo}"

    def find() -> list[str] | None:
        bounds = parse_bounds(s.texts())
        if control in bounds:
            return None
        return [node] if node in bounds else []

    repos = [int(n) for n in re.findall(r"\[APP:READY_REPOS: (\d+)\]", "\n".join(s.texts()))]
    if not repos or repos[-1] < 2:
        return False
    # Repo rows paint after the group header; wait for this repo's node or the row itself.
    deadline = time.monotonic() + timeout
    while (found := find()) == [] and time.monotonic() < deadline:
        time.sleep(0.05)
    if found == []:
        found = scroll_to_change_dirs(s, win, find)
    if not found:
        return False
    needle = f"[APP:REPO_CHANGES_COLLAPSED: {group} {repo} collapsed="
    line = ""
    for _click in range(2):
        box = scroll_into_view(s, win, node)
        before = len(s.lines)
        s.click(win, box)
        _, _, line = s.wait_line(lambda item: needle in item, start=before, timeout=timeout)
        if "collapsed=false" in line:
            return True
    raise NativeBenchError(f"{node} did not open: {line}")


def expand_change_dirs(
    s: NativeSession,
    win: dict[str, Any],
    control: str,
    repo: str,
    timeout: float = 10.0,
) -> list[str]:
    """Open the directories above a Changes file row; returns the ones opened.

    Changes groups files by directory by default, and directories start collapsed.
    Bounds carry no expansion state, so the deepest visible ancestor
    `change-dir:<group>:<repo>:<dir>` is clicked and its CHANGE_DIR_COLLAPSED line read:
    `collapsed=true` means it was already open with its children out of view, so it is
    clicked once more. A compacted chain (`main/java/pkg`) is one directory row.
    """
    path = control.split(":", 2)[2]
    group = change_row_group(control)
    prefix = f"change-dir:{group}:{repo}:"
    expand_change_repo(s, win, control, repo, timeout)
    opened: list[str] = []

    def candidates() -> list[str] | None:
        """Visible unopened ancestors, or None once the row itself is visible."""
        bounds = parse_bounds(s.texts())
        if control in bounds:
            return None
        return [
            key[len(prefix):] for key in bounds
            if key.startswith(prefix) and path.startswith(key[len(prefix):] + "/")
            and key[len(prefix):] not in opened
        ]

    for _ in range(path.rstrip("/").count("/")):
        # Rows under a node that just opened paint on a later frame.
        deadline = time.monotonic() + 2.0
        while (dirs := candidates()) == [] and time.monotonic() < deadline:
            time.sleep(0.05)
        if dirs == []:
            dirs = scroll_to_change_dirs(s, win, candidates)
        if dirs is None:
            return opened
        if not dirs:
            break
        folder = max(dirs, key=len)
        needle = f"[APP:CHANGE_DIR_COLLAPSED: {group} {repo} {folder} files="
        for _click in range(2):
            box = scroll_into_view(s, win, prefix + folder)
            before = len(s.lines)
            s.click(win, box)
            _, _, line = s.wait_line(lambda item: needle in item, start=before, timeout=timeout)
            if "collapsed=false" in line:
                break
        else:
            raise NativeBenchError(f"{prefix + folder} did not open: {line}")
        opened.append(folder)
    return opened


def tab_state(lines: list[str]) -> tuple[str, bool]:
    """Last tool tab. The app starts on Git Changes with the panel open, and does not log that."""
    tabs = [line for line in lines if "[APP:TAB_SWITCHED:" in line]
    if not tabs:
        return "GitChanges", True
    last = tabs[-1]
    if "FileExplorer" in last:
        name = "FileExplorer"
    elif "GitChanges" in last:
        name = "GitChanges"
    else:
        name = "unknown"
    return name, "visible=true" in last


def show_tab(s: NativeSession, win: dict[str, Any], tab: str, rail_id: str) -> None:
    """Open a tool. Clicking the rail of the already-open tool collapses it, so that click is not sent."""
    current, visible = tab_state(s.texts())
    if current == tab and visible:
        return
    bounds = require_control(s.texts(), rail_id)
    assert_on_window(bounds, win, rail_id)
    before = len(s.lines)
    s.click(win, bounds)
    s.wait_line(
        lambda line: f"TAB_SWITCHED: {tab}" in line and "visible=true" in line,
        start=before, timeout=10,
    )


def show_changes(s: NativeSession, win: dict[str, Any]) -> None:
    show_tab(s, win, "GitChanges", "rail-changes")


def open_project_list(s: NativeSession, win: dict[str, Any]) -> None:
    show_tab(s, win, "FileExplorer", "rail-project")
    # TAB_SWITCHED precedes the painted frame, and old repo-row lines are already in the log.
    deadline = time.monotonic() + 10
    while not any(control.startswith("repo-row:") for control in parse_bounds(s.texts())):
        if time.monotonic() >= deadline:
            raise NativeBenchError("no repo-row bounds within 10s of opening the project list")
        time.sleep(0.05)


def open_repo_name(lines: list[str]) -> str | None:
    """Basename of the repo the app last selected, or None after a workspace change."""
    for line in reversed(lines):
        if "[APP:WORKSPACE: state=" in line:
            return None
        if "[APP:REPO_SELECTING:" in line:
            return parse_repo_select(line)[1]
    return None


def narrow_log(s: NativeSession, win: dict[str, Any], name: str, timeout: float = 30.0) -> float | None:
    """The Log shows every repo of the workspace; switching repos keeps it.

    The Repository chip narrows it to `name`, which reads that repo's first
    history page (a fresh E2E_LOG), as a repo switch did before the merged
    log. Returns that page's time, or None for a single-repo workspace.
    """
    if "log-filter-repo" not in parse_bounds(s.texts()):
        return None
    before = len(s.lines)
    s.click(win, parse_bounds(s.texts())["log-filter-repo"])
    s.wait_line(lambda l: "[APP:LOG_MENU: Some(Repo)]" in l, start=before, timeout=timeout)
    row = f"log-repo:{name}"
    deadline = time.monotonic() + timeout
    while row not in parse_bounds(s.texts(before)):
        if time.monotonic() >= deadline:
            raise NativeBenchError(f"Repository chip menu has no {row}")
        time.sleep(0.05)
    s.click(win, parse_bounds(s.texts(before))[row])
    i, _, _ = s.wait_line(lambda l: "[APP:LOG_REPOS: n=1]" in l, start=before, timeout=timeout)
    _, t_page, _ = s.wait_line(lambda l: "[APP:E2E_LOG: mode=graph " in l, start=i, timeout=timeout)
    return t_page


def click_repo(s: NativeSession, win: dict[str, Any], repo_path: str) -> tuple[float, float, float, int]:
    """Click one repo row. Returns click time, load times, and the log index of the click."""
    open_project_list(s, win)
    name = os.path.basename(repo_path)
    assert_current_basket_empty(s.texts(), f"before opening {name}")
    bounds = scroll_into_view(s, win, f"repo-row:{name}")
    assert_on_window(bounds, win, f"repo-row:{name}")
    if open_repo_name(s.texts()) == name:
        # A click on the open, expanded repo collapses it; the second click
        # expands it again, which re-reads it (a fresh select and load).
        s.click(win, bounds)
        time.sleep(0.3)
        bounds = scroll_into_view(s, win, f"repo-row:{name}")
    before = len(s.lines)
    sent = s.click(win, bounds)
    _, _, sel = s.wait_line(lambda line: "[APP:REPO_SELECTING:" in line, start=before, timeout=30)
    idx, got = parse_repo_select(sel)
    if got != name:
        raise NativeBenchError(f"clicked repo-row:{name} but app selected {got} at index {idx}")
    t_loaded, t_graph = wait_repo_loaded(s, repo_path, before)
    narrowed = narrow_log(s, win, name)
    if narrowed is not None:
        t_graph = narrowed
    fresh = s.texts(before)
    bad = [event for event in basket_events(fresh) if event["n"] != 0]
    toggles = [line for line in fresh if "[APP:FILE_TOGGLED:" in line and "selected=true" in line]
    if bad or toggles:
        raise NativeBenchError(f"opening {name} added a basket entry: {(bad or toggles)[-1]}")
    return sent, t_loaded, t_graph, before


def run_soak(s: NativeSession, win: dict[str, Any], repos: list[str], switches: int) -> tuple[dict[str, list[float]], dict[str, Any]]:
    """Clicks repo rows round-robin. Each switch must load, and must not check a file."""
    names = [os.path.basename(r) for r in repos]
    clicks: list[float] = []
    graphs: list[float] = []
    current = None
    checked = 0
    last_state: dict[str, Any] = {}
    for n in range(switches):
        name = names[(n + 1) % len(names)]
        if name == current:
            name = names[(n + 2) % len(names)]
        repo = next(r for r in repos if os.path.basename(r) == name)
        sent, t_loaded, t_graph, before = click_repo(s, win, repo)
        clicks.append((t_loaded - sent) * 1000)
        graphs.append((t_graph - sent) * 1000)
        current = name
        if n % 10 == 0 or n == switches - 1:
            last_state = check_repo_state(s.texts(before), repo_oracle(repo))
            checked += 1
    if len(clicks) != switches:
        raise NativeBenchError(f"soak recorded {len(clicks)} switches, requested {switches}")
    return ({"clickToRepoLoadedMs": clicks, "clickToGraphLoadedMs": graphs},
            {"switches": len(clicks), "oracleChecks": checked, "lastRepo": current,
             "basketEmpty": True, "lastState": last_state})


PROFILE_SETUP = {
    # profile: (workspace kind, --mode, needs SNIP_NATIVE_E2E probes)
    # Bounds exist only when SNIP_NATIVE_E2E=1, so every profile that clicks a control opts in.
    "idle": ("empty", "idle", False),
    "1repo": ("repo", "normal", True),
    "1repo-diff": ("repo", "normal", True),
    "15overview": ("dataset", "overview", True),
    "15active": ("dataset", "normal", True),
    "soak": ("dataset", "normal", True),
}


def run_profile(profile: str, bin_path: str, dataset: str, repo: str, run_dir: str, steady: float, interval: float,
                soak_switches: int, build_profile: str = "unknown") -> dict[str, Any]:
    os.makedirs(run_dir, exist_ok=True)
    kind, mode, e2e = PROFILE_SETUP[profile]
    empty = None
    if kind == "empty":
        # Fixed leaf name: the title bar shows it and the screenshot check reads it back.
        empty = os.path.join(tempfile.mkdtemp(prefix="snip-native-"), "empty-workspace")
        os.makedirs(empty)
    workspace = empty or (repo if kind == "repo" else dataset)
    result: dict[str, Any] = {"profile": profile, "runDir": run_dir, "workspace": workspace, "mode": mode}
    error = None
    s: NativeSession | None = None
    try:
        if profile == "1repo-diff":
            if steady < 30:
                raise NativeBenchError("1repo-diff requires at least 30 steady seconds")
            result["scenario"] = MATCHED_SCENARIO
            result["identityBefore"] = matched_identity(bin_path, repo)
        s = NativeSession(bin_path, workspace, mode, run_dir, e2e)
        result["command"] = s.cmd
        result["isolation"] = s.isolation
        drive(s, profile, workspace, run_dir, steady, interval, soak_switches, result, build_profile=build_profile)
        if profile == "1repo-diff":
            result["identityAfter"] = matched_identity(bin_path, repo)
            if result["identityAfter"] != result["identityBefore"]:
                raise NativeBenchError("binary or participating dataset changed during the matched run")
    except Exception as e:  # noqa: BLE001 - recorded; the run fails after teardown
        error = f"{type(e).__name__}: {e}"
    finally:
        result["cleanupProblems"] = s.stop() if s is not None else []
        if empty:
            shutil.rmtree(os.path.dirname(empty), ignore_errors=True)
    return finalize_run(result, error)


# ---------------------------------------------------------------- report

def markdown(report: dict[str, Any]) -> str:
    meta = report["metadata"]
    has_launch = any("launchSampledPeakRssMib" in prof.get("summary", {}) for prof in report["profiles"].values())
    peak_col = "Launch peak RSS" if has_launch else "Pre-ready peak RSS"
    lines = [
        f"# Native memory run — {meta['binary']['label']}",
        "",
        f"- Binary `{meta['binary']['path']}` sha256 `{meta['binary']['sha256']}` ({meta['binary']['buildProfile']})",
        f"- Source note: {meta['binary']['sourceNote']}",
        _receipt_line(meta["binary"].get("buildReceipt")),
        f"- Harness {meta['harnessRevision']}; steady {report['steadySeconds']} s at {report['sampleIntervalSec']} s; runs/profile {report['runsPerProfile']}",
        f"- {meta['system']['os']} {meta['system']['kernel']}; Xvfb is started with screen argument {SCREEN}; Vulkan {meta['vkDriverFiles']}",
        "- Observed window geometry is per run (`window` from xwininfo), not a screen flag.",
        "",
        f"| Profile | OK | Steady PSS median | Steady RSS median | {peak_col} | Steady peak RSS | Steady CPU % | Latency (median/p95 ms) | E2E probes |",
        "| --- | :-: | --- | --- | --- | --- | --- | --- | :-: |",
    ]
    f = lambda s: f"{s['median']}/{s['p95']}/{s['worst']}" if s else "n/a"  # noqa: E731
    for name, prof in report["profiles"].items():
        if "summary" not in prof:
            lines.append(f"| {name} | {prof['status']} | – | – | – | – | – | – | – |")
            continue
        s = prof["summary"]
        runs = prof["runs"]
        done = [r for r in runs if r["status"] == "COMPLETED"]
        cpu = [r["steadyCpuPercent"] for r in done]
        lat = "; ".join(f"{k} {v['medianMs']}/{v['p95Ms']}" for r in done[:1] for k, v in r["ui"]["latency"].items() if v) or "–"
        peak_val = s.get('launchSampledPeakRssMib') if has_launch else s.get('preReadySampledPeakRssMib')
        lines.append(
            f"| {name} | {s['completedRuns']}/{len(runs)} | {f(s.get('steadyPssMedianMib'))} | {f(s.get('steadyRssMedianMib'))} | "
            f"{f(peak_val)} | {f(s.get('steadySampledPeakRssMib'))} | {f(mib(cpu)) if cpu else 'n/a'} | {lat} | "
            f"{'yes' if PROFILE_SETUP[name][2] else 'no'} |"
        )

    lines += [
        "",
        "Values are median/p95/worst across runs in MiB. Peaks are ~50 ms discrete samples inside the named phase. None is a continuous hardware peak, a first-instruction peak, or a filesystem-cold peak.",
        "",
        "### Profile Semantics & Notes",
        f"- **Artifact status**: {meta['binary']['label']} (build profile `{meta['binary'].get('buildProfile')}`). A profile label or a receipt does not pass the release D4 gate.",
        "- **Release comparison**: this driver does not emit one. `--compare-baseline` exits UNSUPPORTED. The supervisor compares matched run artifacts.",
        "- **Cache**: process-cold (fresh process, private XDG and D-Bus). Filesystem cache is uncontrolled. Filesystem-cold is UNSUPPORTED. This driver does not drop caches.",
        "- **15overview Semantics**: In current prototype, repository 0 is automatically selected upon launch, loading its graph and preview into memory. 15overview reflects 15 discovered repos + 1 loaded active repo; it does not certify summary-only overview memory until app mode defers graph/preview retention. Auto-preview does not fill the basket.",
        "- **1repo-diff**: selected two-commit feature branch on the standard repository; 1080x720 client, fixed tip/file diff, empty basket, unchanged clipboard, no Copy. This optional scenario does not match the default 50/300-row histories or 15 repositories.",
        "- **Explicit copy (other repository profiles)**: one source-aware checkbox, then `btn-copy`. Staged bytes are the index. Unstaged and untracked bytes are the worktree. `files=` is source rows, not distinct paths. A non-empty basket before that click fails the run.",
        "- **History page**: the application built-in length is "
        f"{APPLICATION_HISTORY_PAGE_LENGTH}. There is no history-page CLI. Observed row counts stay on each run.",
        "- **Steady CPU %**: Reflects Mesa lavapipe (llvmpipe) software rasterization overhead on CPU under headless Xvfb.",
        "- **Latencies**: Measured from a real repo-row click to literal log responses. This checkpoint does not bind Alt+2. '–' is idle.",
        "",
        "## Runs",
        "",
    ]
    for name, prof in report["profiles"].items():
        for r in prof.get("runs", []):
            if r["status"] != "COMPLETED":
                lines.append(f"- {name} `{r['runDir']}`: FAILED {r.get('error')}")
                continue
            shot = r["ui"]["screenshot"]
            win = r.get("window") or {}
            geo = ""
            if win.get("width"):
                geo = f"; observed window {win['width']}x{win['height']}+{win.get('x')}+{win.get('y')} {win.get('mapState')}"
            lines.append(
                f"- {name} `{r['runDir']}`: state {json.dumps(r['ui']['state'])}; screenshot `{shot['rootCrop']}` "
                f"(stddev {shot['rootCropStats']['stddev']}; xwd -id stddev {shot['xwdIdStats']['stddev']})"
                f"{geo}; cleanup {r['cleanupProblems'] or 'clean'}"
            )
    return "\n".join(lines) + "\n"


def _receipt_line(receipt: dict[str, Any] | None) -> str:
    if not receipt:
        return "- Build receipt: none recorded"
    checked = "sha256 matches the binary" if receipt.get("sha256MatchesBinary") else "no sha256 field; hash not checked"
    return f"- Build receipt ({receipt.get('origin')}): `{receipt.get('path')}` ({checked})"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True)
    ap.add_argument("--label", required=True, help="Recorded verbatim, e.g. 'DEBUG PILOT ONLY'.")
    ap.add_argument("--build-profile", default="unknown")
    ap.add_argument("--build-receipt", default=None, help="User-supplied build receipt JSON. Recorded as user-supplied. A sha256 field must match --bin or the run is refused.")
    ap.add_argument("--source-note", default="unknown", help="Where the binary came from (tree, revision, dirty state).")
    ap.add_argument("--workspace", required=True, help="Standard dataset dir with workload_manifest.json.")
    ap.add_argument("--repo", default="repo-01-core")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--profile", action="append", choices=PROFILES, help="Repeatable; default legacy profiles (1repo-diff is opt-in).")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--steady-seconds", type=float, default=30.0)
    ap.add_argument("--sample-interval", type=float, default=0.05)
    ap.add_argument("--soak-switches", type=int, default=100)
    ap.add_argument("--compare-baseline", default=None, help="UNSUPPORTED. Does not compare releases; exits nonzero.")
    args = ap.parse_args(argv)
    if args.compare_baseline:
        print(
            "UNSUPPORTED: --compare-baseline does not compare releases. "
            "This flag has no observed per-run dataset fingerprint, viewport, or workload oracle. "
            "The supervisor compares matched run artifacts. The release D4 gate is not passed here.",
            file=sys.stderr,
        )
        return 2
    signal.signal(signal.SIGTERM, lambda signum, frame: sys.exit(128 + signum))

    bin_path = os.path.realpath(args.bin)
    try:
        receipt = load_build_receipt(bin_path, args.build_receipt)
    except (OSError, ValueError) as e:
        print(f"build receipt rejected: {e}", file=sys.stderr)
        return 1
    dataset = os.path.realpath(args.workspace)
    out_dir = os.path.abspath(args.out_dir)
    if os.path.exists(out_dir) and os.listdir(out_dir):
        print(f"refusing to write into non-empty {out_dir}", file=sys.stderr)
        return 1
    os.makedirs(out_dir, exist_ok=True)

    with open(os.path.join(dataset, "workload_manifest.json")) as f:
        manifest = json.load(f)
    report: dict[str, Any] = {
        "timestampUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "metadata": {
            "harnessRevision": HARNESS_REVISION,
            "binary": {"path": bin_path, "sha256": sha256_file(bin_path), "label": args.label,
                       "buildProfile": args.build_profile, "sourceNote": args.source_note,
                       "buildReceipt": receipt},
            "vkDriverFiles": lavapipe_icd(),
            "system": get_system_environment(),
            "xvfb": run_text("Xvfb", "-version"),
            "dataset": {"path": dataset, "workloadRevision": manifest["workloadRevision"], "seed": manifest["seed"],
                        "summary": manifest["summary"]},
            "applicationHistoryPageLength": APPLICATION_HISTORY_PAGE_LENGTH,
            "applicationHistoryPageLengthRole": (
                "built-in of the tested workbench, checked against [APP:GRAPH_LOADED] and "
                "[APP:E2E_LOG]; not a CLI flag, not a viewport, and not a comparison acceptance"
            ),
            "cacheProvenance": {
                "processState": "cold",
                "processIsolation": "fresh process tree, private XDG dirs, private D-Bus session per run",
                "filesystemCache": "uncontrolled",
                "filesystemColdDemonstrated": False,
                "filesystemColdStatus": "UNSUPPORTED (filesystem-cold start requires kernel drop_caches; host page cache is uncontrolled)",
                "runRepeatability": "unpurged host page cache; cold process memory",
            },
            "coldStart": "UNSUPPORTED (process-cold only; filesystem cache uncontrolled; drop_caches not performed)",
        },
        "runsPerProfile": args.runs,
        "steadySeconds": args.steady_seconds,
        "sampleIntervalSec": args.sample_interval,
        "profiles": {},
    }
    failed = False
    for profile in args.profile or list(DEFAULT_PROFILES):
        runs = []
        for i in range(1, args.runs + 1):
            run_dir = os.path.join(out_dir, profile, f"run-{i:02d}")
            print(f"[{profile} {i}/{args.runs}] {run_dir}", flush=True)
            r = run_profile(profile, bin_path, dataset, os.path.join(dataset, args.repo), run_dir, args.steady_seconds,
                            args.sample_interval, args.soak_switches, build_profile=args.build_profile)
            if r["status"] == "COMPLETED":
                m = r["measurement"]["steadyMetrics"]
                print(f"  steady PSS {m['pssMedianMib']} MiB, RSS {m['rssMedianMib']} MiB, CPU {r['steadyCpuPercent']}%", flush=True)
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
    report["status"] = "FAILED" if failed else "COMPLETED"
    with open(os.path.join(out_dir, "native_report.json"), "w") as f:
        json.dump(report, f, indent=2)
    with open(os.path.join(out_dir, "native_report.md"), "w") as f:
        f.write(markdown(report))
    print(f"report: {out_dir}/native_report.md")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
