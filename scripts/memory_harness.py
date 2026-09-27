#!/usr/bin/env python3
"""
memory_harness.py - Linux Memory & Footprint Benchmark Harness for snip-sync.

Designed according to docs/native-git-workbench-plan.md sections 9-10.
Measures process tree memory footprint (RSS, PSS, main VmHWM) with:
- Continuous high-frequency sampling (~50ms) using Python stdlib /proc readers
- Literal string readiness marker detection across bounded chunks (not regex)
- Spawn mode: launch until the readiness marker, then >=30s steady state.
  Attach mode is pre-ready unless a sampler gate was published before the
  expected executable and a later tick observed that exec transition.
- Strict process group cleanup on all exit paths (SIGTERM -> SIGKILL) with signal handling
- Honest provenance: PSS is null if smaps_rollup unavailable; main VmHWM is never summed
- Strict failure on missing readiness, crash, or missing binary (no successful zeros)
- Bounded rolling log buffers (debug output cannot exhaust harness RAM)
- Raw JSONL samples + comprehensive metadata JSON + Markdown summary table
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any

HARNESS_REVISION = "2026-09-26.2"
# The ready file only ever holds the marker; never read more than this.
READY_FILE_MAX_BYTES = 4096


class HarnessError(Exception):
    """Base error for memory harness failures."""
    pass


class BinaryNotFoundError(HarnessError):
    pass


class ReadinessTimeoutError(HarnessError):
    pass


class ProcessCrashedError(HarnessError):
    pass


class ZeroSamplesError(HarnessError):
    pass


def read_proc_starttime(pid: int, proc_root: str = "/proc") -> int | None:
    """Process start time in clock ticks (/proc/<pid>/stat field 22); None if gone or a zombie.

    (pid, starttime) identifies a process across PID reuse.
    """
    try:
        with open(os.path.join(proc_root, str(pid), "stat"), "r") as f:
            stat = f.read()
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return None
    # comm (field 2) may contain spaces or ')', so split after the last ')'.
    fields = stat[stat.rfind(")") + 2:].split()
    if fields and fields[0] in ("Z", "X"):
        return None
    try:
        return int(fields[19])
    except (IndexError, ValueError):
        return None


def calculate_p95(values: list[float | int]) -> float:
    """Calculates 95th percentile using linear interpolation."""
    if not values:
        return 0.0
    s = sorted(values)
    if len(s) == 1:
        return float(s[0])
    k = (len(s) - 1) * 0.95
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return float(s[int(k)])
    return float(s[int(f)] * (c - k) + s[int(c)] * (k - f))


class BoundedOutputMonitor:
    """
    Monitors process stdout/stderr stream in bounded chunks.
    Maintains a rolling tail for literal readiness marker detection (even across chunk boundaries)
    and retains a bounded diagnostic error buffer (max 64 KB) to avoid unbounded harness RAM.
    """

    def __init__(self, marker: str, max_log_bytes: int = 65536):
        self.marker = marker
        self.max_tail_chars = max(len(marker) * 4, 8192)
        self.max_log_bytes = max_log_bytes
        self.lock = threading.Lock()
        self.rolling_tail = ""
        self.marker_found = False
        self.log_chunks: list[str] = []
        self.log_bytes_total = 0

    def add_chunk(self, chunk: str) -> None:
        if not chunk:
            return
        with self.lock:
            # Truncate chunk if individually larger than max_log_bytes
            if len(chunk) > self.max_log_bytes:
                chunk = chunk[-self.max_log_bytes:]

            # 1. Update diagnostic ring buffer
            chunk_len = len(chunk)
            self.log_chunks.append(chunk)
            self.log_bytes_total += chunk_len
            while self.log_bytes_total > self.max_log_bytes and len(self.log_chunks) > 0:
                removed = self.log_chunks.pop(0)
                self.log_bytes_total -= len(removed)

            # 2. Literal marker detection across rolling boundary
            if not self.marker_found:
                self.rolling_tail += chunk
                if self.marker in self.rolling_tail:
                    self.marker_found = True
                    self.rolling_tail = ""  # Reclaim memory once matched
                elif len(self.rolling_tail) > self.max_tail_chars:
                    self.rolling_tail = self.rolling_tail[-self.max_tail_chars:]

    def is_marker_found(self) -> bool:
        with self.lock:
            return self.marker_found

    def get_diagnostic_log(self) -> str:
        with self.lock:
            return "".join(self.log_chunks)


class ProcessTreeSampler:
    """Discovers and samples the process tree rooted at root_pid using Linux /proc."""

    def __init__(self, root_pid: int, pgrp: int | None = None, proc_root: str = "/proc"):
        self.root_pid = root_pid
        self.pgrp = pgrp
        self.proc_root = proc_root
        try:
            self.page_size = os.sysconf("SC_PAGE_SIZE")
        except Exception:
            self.page_size = 4096

    def get_tree_pids(self, strict_children: bool = False) -> tuple[list[int], list[int]]:
        """
        Discovers all live PIDs in the process tree.
        Returns: (live_pids, unreadable_or_missing_child_pids)

        strict_children records a missing or unreadable task/children file instead of
        treating that process as childless. The legacy default keeps the historical swallow.
        """
        pids: list[int] = [self.root_pid]
        visited: set[int] = {self.root_pid}
        queue: list[int] = [self.root_pid]
        unreadable_children: list[int] = []

        def note(pid: int) -> None:
            if pid not in unreadable_children:
                unreadable_children.append(pid)

        # 1. Breadth-first traversal through /proc/<pid>/task/<tid>/children
        while queue:
            curr = queue.pop(0)
            task_dir = os.path.join(self.proc_root, str(curr), "task")
            try:
                if not os.path.isdir(task_dir):
                    if strict_children:
                        note(curr)
                    continue
                tids = os.listdir(task_dir)
                if strict_children and not tids:
                    note(curr)
                for tid in tids:
                    children_path = os.path.join(task_dir, tid, "children")
                    if strict_children and not os.path.isfile(children_path):
                        if os.path.exists(os.path.join(task_dir, tid)):
                            note(curr)
                        continue
                    try:
                        with open(children_path, "r") as f:
                            for child_str in f.read().split():
                                c_pid = int(child_str)
                                if c_pid not in visited:
                                    visited.add(c_pid)
                                    if os.path.exists(os.path.join(self.proc_root, str(c_pid))):
                                        queue.append(c_pid)
                                        pids.append(c_pid)
                                    else:
                                        note(c_pid)
                    except (FileNotFoundError, ProcessLookupError):
                        if strict_children and os.path.exists(os.path.join(task_dir, tid)):
                            note(curr)
                    except PermissionError:
                        if strict_children:
                            note(curr)
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                if strict_children:
                    note(curr)

        # 2. Safety scan for any other processes belonging to the same process group
        if self.pgrp is not None:
            try:
                for entry in os.listdir(self.proc_root):
                    if entry.isdigit():
                        pid = int(entry)
                        if pid not in visited:
                            try:
                                if os.getpgid(pid) == self.pgrp:
                                    visited.add(pid)
                                    pids.append(pid)
                            except (ProcessLookupError, PermissionError, OSError):
                                pass
            except Exception:
                pass

        return pids, unreadable_children

    def sample_process_memory(self, pid: int) -> tuple[int | None, int | None, str, str | None]:
        """
        Reads RSS and PSS for a single PID.
        Returns: (rss_bytes, pss_bytes, rss_provenance, pss_provenance)
        Provenance: 'smaps_rollup', 'statm', or 'none'.
        If PSS is unavailable from smaps_rollup, pss_bytes MUST be None.
        """
        smaps_rollup_path = os.path.join(self.proc_root, str(pid), "smaps_rollup")
        try:
            with open(smaps_rollup_path, "r") as f:
                rss_kb: int | None = None
                pss_kb: int | None = None
                for line in f:
                    if line.startswith("Rss:"):
                        parts = line.split()
                        if len(parts) >= 2:
                            rss_kb = int(parts[1])
                    elif line.startswith("Pss:"):
                        parts = line.split()
                        if len(parts) >= 2:
                            pss_kb = int(parts[1])
                    if rss_kb is not None and pss_kb is not None:
                        break

                if rss_kb is not None:
                    rss_bytes = rss_kb * 1024
                    pss_bytes = (pss_kb * 1024) if pss_kb is not None else None
                    return (rss_bytes, pss_bytes, "smaps_rollup", "smaps_rollup" if pss_kb is not None else None)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass

        # Fallback to /proc/<pid>/statm for RSS only. PSS MUST REMAIN NULL.
        statm_path = os.path.join(self.proc_root, str(pid), "statm")
        try:
            with open(statm_path, "r") as f:
                tokens = f.read().split()
                if len(tokens) >= 2:
                    resident_pages = int(tokens[1])
                    return (resident_pages * self.page_size, None, "statm", None)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass

        return (None, None, "none", None)

    def sample_tree(self) -> dict[str, Any]:
        """
        Samples entire process tree.
        Ensures valid root sample with positive RSS; explicitly records unreadable children.
        """
        # First sample root process
        root_rss, root_pss, root_r_prov, root_p_prov = self.sample_process_memory(self.root_pid)
        root_sampled = (root_rss is not None and root_rss > 0)

        pids, unreadable_children_pids = self.get_tree_pids()
        total_rss = 0
        total_pss = 0
        all_pss_available = True
        sampled_pids: list[int] = []
        per_process: list[dict[str, int | None]] = []
        rss_prov_set: set[str] = set()
        pss_prov_set: set[str] = set()

        for pid in pids:
            if pid == self.root_pid:
                rss_b, pss_b, r_prov, p_prov = root_rss, root_pss, root_r_prov, root_p_prov
            else:
                rss_b, pss_b, r_prov, p_prov = self.sample_process_memory(pid)

            if rss_b is not None and rss_b > 0:
                total_rss += rss_b
                sampled_pids.append(pid)
                per_process.append({"pid": pid, "rssBytes": rss_b, "pssBytes": pss_b})
                rss_prov_set.add(r_prov)
                if pss_b is not None:
                    total_pss += pss_b
                    if p_prov:
                        pss_prov_set.add(p_prov)
                else:
                    all_pss_available = False
            else:
                if pid != self.root_pid:
                    unreadable_children_pids.append(pid)

        # Sample is valid only if root was successfully sampled with positive RSS
        sample_valid = root_sampled and total_rss > 0

        rss_provenance = ",".join(sorted(rss_prov_set)) if rss_prov_set else "none"
        pss_provenance = ",".join(sorted(pss_prov_set)) if (pss_prov_set and all_pss_available) else None

        return {
            "valid": sample_valid,
            "rootSampled": root_sampled,
            "rootRssBytes": root_rss,
            "unreadableChildrenPids": unreadable_children_pids,
            "pids": sampled_pids,
            "perProcess": per_process,
            "processCount": len(sampled_pids),
            "totalRssBytes": total_rss,
            "totalPssBytes": total_pss if (all_pss_available and sampled_pids) else None,
            "pssAvailable": all_pss_available and bool(sampled_pids),
            "provenance": {
                "rss": rss_provenance,
                "pss": pss_provenance,
            },
        }

    def read_root_vm_hwm(self) -> tuple[int | None, str]:
        """
        Reads VmHWM (Peak Resident Set Size) for root process from /proc/<pid>/status.
        Returns: (vm_hwm_bytes, provenance).
        """
        status_path = os.path.join(self.proc_root, str(self.root_pid), "status")
        try:
            with open(status_path, "r") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        parts = line.split()
                        if len(parts) >= 2:
                            return (int(parts[1]) * 1024, "status_vmhwm")
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass
        return (None, "none")


# Controller and Xvfb are not the app. Their /proc RSS/PSS must not enter the app total.
CONTROLLER_PROCESS_NAMES = frozenset({"Xvfb", "Xvfb-run", "dbus-run-session", "dbus-daemon"})
GPU_NOT_MEASURED = (
    "Linux /proc RSS/PSS do not include GPU VRAM, DMA-BUF, or X11/Wayland compositor surfaces. "
    "This sampler does not estimate them and never adds them to RSS or PSS."
)


def read_proc_comm(pid: int, proc_root: str = "/proc") -> str | None:
    try:
        with open(os.path.join(proc_root, str(pid), "comm"), "r") as f:
            return f.read().strip()
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None


def count_open_fds(pid: int, proc_root: str = "/proc") -> int | None:
    """Number of entries in /proc/<pid>/fd. None if the directory cannot be read."""
    fd_dir = os.path.join(proc_root, str(pid), "fd")
    try:
        return len(os.listdir(fd_dir))
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None


def count_inotify_watches(pid: int, proc_root: str = "/proc") -> int | None:
    """Inotify watches from /proc/<pid>/fdinfo. None if fd or fdinfo cannot be read.

    A readable tree with no `inotify ` lines is 0. An unreadable fdinfo is not reported as 0.
    """
    fd_dir = os.path.join(proc_root, str(pid), "fd")
    try:
        fds = [name for name in os.listdir(fd_dir) if name.isdigit()]
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None
    total = 0
    for fd in fds:
        info_path = os.path.join(proc_root, str(pid), "fdinfo", fd)
        try:
            with open(info_path, "r") as handle:
                text = handle.read()
        except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
            return None
        total += sum(1 for line in text.splitlines() if line.startswith("inotify "))
    return total


def count_threads(pid: int, proc_root: str = "/proc") -> int | None:
    """Kernel thread count from /proc/<pid>/status, else the task directory. None if unread."""
    status_path = os.path.join(proc_root, str(pid), "status")
    try:
        with open(status_path, "r") as f:
            for line in f:
                if line.startswith("Threads:"):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError, IndexError, ValueError):
        pass
    task_dir = os.path.join(proc_root, str(pid), "task")
    try:
        names = [name for name in os.listdir(task_dir) if name.isdigit()]
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None
    return len(names) if names else None


def _controller_reason(ident: dict[str, Any], exclude_pids: set[int]) -> str | None:
    if ident.get("pid") in exclude_pids:
        return "exclude-pid"
    comm = ident.get("comm") or ""
    exe = ident.get("exe") or ""
    base = os.path.basename(exe) if exe else ""
    if comm in CONTROLLER_PROCESS_NAMES or base in CONTROLLER_PROCESS_NAMES:
        return "controller-or-xvfb"
    return None


def _is_git_process(ident: dict[str, Any]) -> bool:
    comm = ident.get("comm") or ""
    exe = ident.get("exe") or ""
    base = os.path.basename(exe) if exe else ""
    return comm == "git" or comm.startswith("git-") or base == "git"


def _finite_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def sample_app_resources(
    root_pid: int,
    *,
    expected_exe: str,
    expected_starttime: int,
    exclude_pids: set[int] | None = None,
    proc_root: str = "/proc",
) -> dict[str, Any]:
    """One app-tree RSS/PSS/fd/thread sample rooted at an exact (exe, starttime).

    Controller and Xvfb processes are listed under `excluded` and are not summed.
    GPU memory is not estimated. A missing RSS or PSS stays null; it is never replaced with 0.
    """
    excluded_ids = set(exclude_pids or ())
    gpu = {
        "vramBytes": None,
        "sharedEstimateBytes": None,
        "combinedIntoRss": False,
        "reason": GPU_NOT_MEASURED,
    }
    root = read_process_identity(root_pid, proc_root)
    if root is None:
        return {
            "valid": False,
            "resourcesComplete": False,
            "memoryComplete": False,
            "rootIdentityMatches": False,
            "reasons": ["root process is not alive"],
            "root": None,
            "included": [],
            "excluded": [],
            "unreadablePids": [root_pid],
            "totals": {
                "rssBytes": None,
                "pssBytes": None,
                "fdCount": None,
                "threadCount": None,
                "watchCount": None,
                "processCount": 0,
                "gitChildren": 0,
            },
            "activity": "unreadable",
            "gpu": gpu,
        }
    root["comm"] = read_proc_comm(root_pid, proc_root)
    try:
        expected_real = os.path.realpath(expected_exe)
    except OSError:
        expected_real = expected_exe
    identity_ok = root.get("exe") == expected_real and root.get("starttime") == expected_starttime
    reasons: list[str] = []
    root_link = read_exe_link(root_pid, proc_root)
    if isinstance(root_link, str) and root_link.endswith(" (deleted)"):
        identity_ok = False
        reasons.append("root executable deleted during sample")
    elif not identity_ok:
        reasons.append("root identity mismatch")
    if _controller_reason(root, excluded_ids):
        reasons.append("root is controller or Xvfb")

    sampler = ProcessTreeSampler(root_pid, None, proc_root)
    pids, unreadable = sampler.get_tree_pids(strict_children=True)
    included: list[dict[str, Any]] = []
    excluded_rows: list[dict[str, Any]] = []
    incomplete = False
    if unreadable:
        incomplete = True
        reasons.append("unreadable process-tree evidence")
    for pid in pids:
        ident = read_process_identity(pid, proc_root)
        if ident is None:
            unreadable.append(pid)
            incomplete = True
            continue
        ident["comm"] = read_proc_comm(pid, proc_root)
        rss, pss, rss_prov, pss_prov = sampler.sample_process_memory(pid)
        fd_count = count_open_fds(pid, proc_root)
        thread_count = count_threads(pid, proc_root)
        watches = count_inotify_watches(pid, proc_root)
        after = read_process_identity(pid, proc_root)
        link = read_exe_link(pid, proc_root)
        deleted = isinstance(link, str) and link.endswith(" (deleted)")
        if (
            after is None
            or link is None
            or deleted
            or after.get("starttime") != ident.get("starttime")
            or after.get("exe") != ident.get("exe")
        ):
            unreadable.append(pid)
            incomplete = True
            reasons.append("executable deleted during sample" if deleted else "pid identity changed during sample")
            continue
        row: dict[str, Any] = {
            **after,
            "comm": ident["comm"],
            "rssBytes": rss,
            "pssBytes": pss,
            "fdCount": fd_count,
            "threadCount": thread_count,
            "watchCount": watches,
            "rssProvenance": rss_prov,
            "pssProvenance": pss_prov,
        }
        why = _controller_reason(ident, excluded_ids)
        if why:
            row["excludeReason"] = why
            excluded_rows.append(row)
            continue
        if not (isinstance(rss, int) and not isinstance(rss, bool) and rss > 0):
            incomplete = True
        if pss is None or not _finite_number(pss) or pss < 0:
            incomplete = True
        if row["fdCount"] is None or row["threadCount"] is None or watches is None:
            incomplete = True
        elif row["threadCount"] < 1 or row["fdCount"] < 0 or watches < 0:
            incomplete = True
        included.append(row)
    end_root = read_process_identity(root_pid, proc_root)
    end_link = read_exe_link(root_pid, proc_root)
    end_deleted = isinstance(end_link, str) and end_link.endswith(" (deleted)")
    if (
        end_root is None
        or end_link is None
        or end_deleted
        or end_root.get("starttime") != root.get("starttime")
        or end_root.get("exe") != root.get("exe")
        or end_root.get("pid") != root_pid
    ):
        incomplete = True
        reasons.append("root executable deleted during sample" if end_deleted else "root identity changed during sample")

    def _sum(key: str) -> int | None:
        values = [row[key] for row in included]
        if not values or any(not _finite_number(value) or value < 0 for value in values):
            return None
        return int(sum(values))

    enumeration_failed = bool(unreadable) or any(
        "unreadable" in reason or "changed during sample" in reason or "deleted" in reason for reason in reasons
    )
    git_children = None if enumeration_failed else sum(1 for row in included if row["pid"] != root_pid and _is_git_process(row))
    if git_children is None:
        activity = "unreadable"
    elif git_children:
        activity = "busy"
    else:
        activity = "quiescent"
    provenance_rows = included
    rss_provenance = provenance_rows[0].get("rssProvenance") if len({row.get("rssProvenance") for row in provenance_rows}) == 1 else None
    pss_provenance = provenance_rows[0].get("pssProvenance") if len({row.get("pssProvenance") for row in provenance_rows}) == 1 else None
    rss_total = _sum("rssBytes")
    pss_total = _sum("pssBytes")
    fd_total = _sum("fdCount")
    thread_total = _sum("threadCount")
    watch_values = [row.get("watchCount") for row in included]
    watch_total = int(sum(watch_values)) if watch_values and all(_finite_number(value) and value >= 0 for value in watch_values) else None
    memory_complete = (
        not incomplete
        and not reasons
        and rss_total is not None
        and rss_total > 0
        and pss_total is not None
        and pss_total > 0
        and bool(included)
    )
    resources_complete = (
        memory_complete
        and fd_total is not None
        and thread_total is not None
        and thread_total >= 1
        and watch_total is not None
        and git_children is not None
    )
    return {
        "valid": resources_complete,
        "resourcesComplete": resources_complete,
        "memoryComplete": memory_complete,
        "rootIdentityMatches": identity_ok,
        "reasons": reasons,
        "root": {"pid": root["pid"], "starttime": root["starttime"], "exe": root.get("exe"), "comm": root.get("comm")},
        "included": included,
        "excluded": excluded_rows,
        "unreadablePids": unreadable,
        "totals": {
            "rssBytes": rss_total if memory_complete else None,
            "pssBytes": pss_total if memory_complete else None,
            "fdCount": fd_total if resources_complete else None,
            "threadCount": thread_total if resources_complete else None,
            "watchCount": watch_total if resources_complete else None,
            "processCount": len(included),
            "gitChildren": git_children,
        },
        "activity": activity,
        "rssProvenance": rss_provenance,
        "pssProvenance": pss_provenance,
        "gpu": gpu,
    }


def cleanup_process_group(pgrp: int, proc: subprocess.Popen | None = None) -> None:
    """
    Strictly terminates all processes in the process group via SIGTERM followed by SIGKILL.
    Raises HarnessError if child processes cannot be reaped.
    """
    try:
        os.killpg(pgrp, signal.SIGTERM)
    except (ProcessLookupError, PermissionError, OSError):
        pass

    # Wait up to 0.3s for cooperative exit
    t0 = time.monotonic()
    while time.monotonic() - t0 < 0.3:
        try:
            os.killpg(pgrp, 0)
            time.sleep(0.02)
        except ProcessLookupError:
            break
        except Exception:
            break

    # Force kill any remaining processes in group
    try:
        os.killpg(pgrp, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass

    # Wait for process reap
    if proc is not None:
        reaped = False
        try:
            proc.wait(timeout=0.5)
            reaped = True
        except subprocess.TimeoutExpired:
            try:
                os.kill(proc.pid, signal.SIGKILL)
                proc.wait(timeout=0.5)
                reaped = True
            except Exception:
                pass
        except Exception:
            pass

        if not reaped and proc.poll() is None:
            raise HarnessError(
                f"Cleanup failure: Process {proc.pid} in process group {pgrp} could not be reaped after SIGKILL."
            )

    # Verify no remaining active processes in pgrp
    try:
        os.killpg(pgrp, 0)
        raise HarnessError(
            f"Cleanup failure: Active processes still remain in process group {pgrp} after SIGKILL."
        )
    except ProcessLookupError:
        pass
    except PermissionError:
        pass


def get_system_environment() -> dict[str, Any]:
    """Gathers OS and hardware environment metadata."""
    os_desc = "Linux"
    if os.path.exists("/etc/os-release"):
        try:
            with open("/etc/os-release") as f:
                for line in f:
                    if line.startswith("PRETTY_NAME="):
                        os_desc = line.split("=", 1)[1].strip().strip('"')
                        break
        except Exception:
            pass

    kernel = os.uname().release
    cpu_model = "Unknown CPU"
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    cpu_model = line.split(":", 1)[1].strip()
                    break
    except Exception:
        pass

    mem_total_bytes = 0
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    parts = line.split()
                    if len(parts) >= 2:
                        mem_total_bytes = int(parts[1]) * 1024
                        break
    except Exception:
        pass

    return {
        "os": os_desc,
        "kernel": kernel,
        "cpu": cpu_model,
        "memoryTotalBytes": mem_total_bytes,
        "memoryTotalMib": round(mem_total_bytes / (1024 * 1024), 2),
        "display": os.environ.get("DISPLAY", "none"),
    }


def get_proc_exe(pid: int, proc_root: str = "/proc") -> str | None:
    """Reads /proc/<pid>/exe realpath, handling '(deleted)' markers and errors cleanly."""
    try:
        target = os.readlink(os.path.join(proc_root, str(pid), "exe"))
        if target.endswith(" (deleted)"):
            target = target[:-10]
        return os.path.realpath(target)
    except OSError:
        return None


def read_exe_link(pid: int, proc_root: str = "/proc") -> str | None:
    """Raw /proc/<pid>/exe link, including a trailing ' (deleted)' marker."""
    try:
        return os.readlink(os.path.join(proc_root, str(pid), "exe"))
    except OSError:
        return None


def read_process_identity(pid: int, proc_root: str = "/proc") -> dict[str, Any] | None:
    """(pid, starttime, exe) or None if the process is gone or a zombie."""
    start = read_proc_starttime(pid, proc_root)
    if start is None:
        return None
    return {"pid": pid, "starttime": start, "exe": get_proc_exe(pid, proc_root)}


def _sample_phase(
    *,
    ready: bool,
    exe: str | None,
    expected: str | None,
    pre_exec_gate: bool,
    transition_seen: bool,
    spawned: bool,
) -> str:
    """Phase label for one stable sample.

    Launch requires a gate published while exe was not the expected binary and a
    later observation that it became that binary. A matching exe at attach is
    pre-ready. Spawn mode without an expected exe keeps the historical launch label.
    """
    if expected is not None and exe != expected:
        return "launcher-setup"
    if ready:
        return "steady"
    if expected is not None:
        if pre_exec_gate and transition_seen:
            return "launch"
        return "pre-ready"
    return "launch" if spawned else "pre-ready"


def measure_single_profile(
    argv: list[str] | None = None,
    ready_marker: str = "",
    profile_label: str = "Benchmark",
    steady_seconds: float = 30.0,
    sample_interval: float = 0.050,
    readiness_timeout: float = 15.0,
    env_overrides: dict[str, str] | None = None,
    workload_revision: str = HARNESS_REVISION,
    attach_pid: int | None = None,
    ready_file: str | None = None,
    attach_starttime: int | None = None,
    sampler_ready_file: str | None = None,
    expected_exe: str | None = None,
) -> dict[str, Any]:
    """
    Executes a single benchmark run against an explicit argv array or attached PID.
    Spawn mode without expected_exe samples launch through the readiness marker, then steady.
    Attach mode never signals the attached process. It is pre-ready unless sampler_ready_file
    was published while /proc/<pid>/exe was not expected_exe and a later tick saw that exe.
    Identity is read before and after every sample. A changed or unreadable exe is excluded.
    An unexpected exe, a missing expected target, or a readiness marker on the wrong exe fails.
    """
    if attach_pid is not None:
        if attach_pid <= 0:
            raise ValueError(f"attach_pid must be a positive integer, got {attach_pid}")
        current_start = read_proc_starttime(attach_pid)
        if current_start is None:
            raise BinaryNotFoundError(f"Attached PID {attach_pid} does not exist in /proc.")
        if attach_starttime is not None and attach_starttime != current_start:
            raise ProcessCrashedError(
                f"Attached PID {attach_pid} has starttime {current_start}, expected {attach_starttime}: PID was reused."
            )
        attach_starttime = current_start
        effective_argv = argv if (argv and argv[0]) else [f"(attached-pid:{attach_pid})"]
        bin_path = None
    else:
        if not argv or not argv[0]:
            raise BinaryNotFoundError("No executable binary provided in argv.")
        effective_argv = argv
        bin_path = argv[0]
        if not shutil.which(bin_path) and not os.path.isfile(bin_path):
            raise BinaryNotFoundError(f"Binary not found or not executable: {bin_path}")

    # Validation: non-empty marker and finite positive timing parameters
    if not ready_marker or not ready_marker.strip():
        raise ValueError("Readiness marker must be a non-empty string.")

    if not math.isfinite(sample_interval) or sample_interval <= 0:
        raise ValueError(f"sample_interval must be a positive finite number, got {sample_interval}")

    if not math.isfinite(steady_seconds) or steady_seconds <= 0:
        raise ValueError(f"steady_seconds must be a positive finite number, got {steady_seconds}")

    if not math.isfinite(readiness_timeout) or readiness_timeout <= 0:
        raise ValueError(f"readiness_timeout must be a positive finite number, got {readiness_timeout}")

    ready_file_path = os.path.abspath(ready_file) if ready_file else None
    if ready_file_path and os.path.lexists(ready_file_path):
        raise HarnessError(f"Ready file {ready_file_path} already exists before the run: refusing a stale marker.")

    sampler_ready_path = os.path.abspath(sampler_ready_file) if sampler_ready_file else None
    if sampler_ready_path and os.path.lexists(sampler_ready_path):
        raise HarnessError(f"Sampler ready file {sampler_ready_path} already exists before the run: refusing a stale signal.")

    start_iso = datetime.datetime.now(datetime.timezone.utc).isoformat()
    start_monotonic = time.monotonic()
    deadline_monotonic = start_monotonic + readiness_timeout + steady_seconds + 10.0

    # Setup environment
    exec_env = os.environ.copy()
    if env_overrides:
        exec_env.update(env_overrides)

    proc: subprocess.Popen | None = None
    if attach_pid is not None:
        root_pid = attach_pid
        pgrp = None
        sampler = ProcessTreeSampler(root_pid, pgrp)
    else:
        # Spawn process in a new process group (setsid)
        try:
            proc = subprocess.Popen(
                effective_argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
                bufsize=0,
                env=exec_env,
            )
        except Exception as e:
            raise BinaryNotFoundError(f"Failed to start binary '{bin_path}': {e}") from e

        root_pid = proc.pid
        pgrp = os.getpgid(root_pid)
        sampler = ProcessTreeSampler(root_pid, pgrp)

    # Output monitor with bounded rolling buffer (prevents RAM blowup on verbose debug output)
    monitor = BoundedOutputMonitor(ready_marker)

    def stream_chunk_reader(pipe: Any) -> None:
        fd = pipe.fileno()
        try:
            while True:
                raw_bytes = os.read(fd, 4096)
                if not raw_bytes:
                    break
                text = raw_bytes.decode("utf-8", errors="replace")
                monitor.add_chunk(text)
        except (OSError, ValueError):
            pass
        finally:
            try:
                pipe.close()
            except Exception:
                pass

    t_out: threading.Thread | None = None
    t_err: threading.Thread | None = None
    if proc is not None:
        t_out = threading.Thread(target=stream_chunk_reader, args=(proc.stdout,), daemon=True)
        t_err = threading.Thread(target=stream_chunk_reader, args=(proc.stderr,), daemon=True)
        t_out.start()
        t_err.start()

    # Install signal handlers for SIGTERM and SIGINT to guarantee process group cleanup
    original_sigterm = signal.getsignal(signal.SIGTERM)
    original_sigint = signal.getsignal(signal.SIGINT)

    def sig_cleanup_handler(signum: int, frame: Any) -> None:
        if pgrp is not None and proc is not None:
            cleanup_process_group(pgrp, proc)
        sys.exit(128 + signum)

    signal.signal(signal.SIGTERM, sig_cleanup_handler)
    signal.signal(signal.SIGINT, sig_cleanup_handler)

    ready_monotonic: float | None = None
    samples: list[dict[str, Any]] = []
    unreadable_root_count = 0

    expected_exe_real = os.path.realpath(expected_exe) if expected_exe else None
    exec_discovered_monotonic: float | None = None
    pre_exec_gate = False
    gate_published = False
    gate_exe: str | None = None
    initial_exe: str | None = None
    target_seen = False
    excluded_uncertainty = 0
    tracked_starttime = attach_starttime

    last_sample_time = start_monotonic
    sample_index = 0

    def never_exec_suffix() -> str:
        if expected_exe_real is not None and exec_discovered_monotonic is None and not target_seen:
            return f" Target never executed expected binary '{expected_exe}'."
        return ""

    def marker_visible() -> bool:
        if ready_file_path and os.path.exists(ready_file_path):
            try:
                with open(ready_file_path, "r", encoding="utf-8", errors="replace") as rf:
                    if ready_marker in rf.read(READY_FILE_MAX_BYTES):
                        return True
            except OSError:
                pass
        return monitor.is_marker_found()

    try:
        if proc is not None and tracked_starttime is None:
            tracked_starttime = read_proc_starttime(root_pid, sampler.proc_root)

        # Handlers, sampler, and a readable identity are in place before the gate unblocks a waiter.
        if sampler_ready_path:
            ident = read_process_identity(root_pid, sampler.proc_root)
            if ident is None or (tracked_starttime is not None and ident["starttime"] != tracked_starttime):
                raise ProcessCrashedError(
                    f"Process PID {root_pid} exited before the sampler gate was published."
                    + never_exec_suffix()
                )
            if tracked_starttime is None:
                tracked_starttime = ident["starttime"]
            if expected_exe_real is not None and ident["exe"] is None:
                raise HarnessError(
                    "Refusing to publish the sampler gate: /proc exe is unreadable, "
                    "so a later readiness marker would not prove which binary ran."
                )
            gate_exe = ident["exe"]
            initial_exe = gate_exe
            pre_exec_gate = expected_exe_real is not None and gate_exe != expected_exe_real
            tmp_ready = sampler_ready_path + ".tmp"
            try:
                with open(tmp_ready, "w", encoding="utf-8") as f:
                    f.write(f"SAMPLER_READY pid={root_pid}\n")
                    f.flush()
                    os.fsync(f.fileno())
                os.replace(tmp_ready, sampler_ready_path)
            finally:
                if os.path.lexists(tmp_ready):
                    try:
                        os.remove(tmp_ready)
                    except OSError:
                        pass
            gate_published = True
            start_monotonic = time.monotonic()
            deadline_monotonic = start_monotonic + readiness_timeout + steady_seconds + 10.0
            last_sample_time = start_monotonic

        while True:
            t_now = time.monotonic()

            if t_now > deadline_monotonic:
                diag = monitor.get_diagnostic_log()
                if pgrp is not None and proc is not None:
                    cleanup_process_group(pgrp, proc)
                msg = f"Overall deadline of {deadline_monotonic - start_monotonic:.1f}s exceeded for profile '{profile_label}'."
                raise ReadinessTimeoutError(f"{msg}{never_exec_suffix()}\nDiagnostic log tail:\n{diag}")

            if proc is not None:
                return_code = proc.poll()
                if return_code is not None:
                    time.sleep(0.05)
                    diag = monitor.get_diagnostic_log()
                    cleanup_process_group(pgrp, proc)
                    raise ProcessCrashedError(
                        f"Process (PID {root_pid}, label '{profile_label}') terminated prematurely "
                        f"with exit code {return_code}.{never_exec_suffix()}\n"
                        f"Diagnostic log tail:\n{diag}"
                    )

            before = read_process_identity(root_pid, sampler.proc_root)
            if before is None or (tracked_starttime is not None and before["starttime"] != tracked_starttime):
                if proc is None:
                    msg = (
                        f"Attached process (PID {root_pid}, starttime {tracked_starttime}, "
                        f"label '{profile_label}') exited or was reused."
                    )
                else:
                    msg = f"Process PID {root_pid} exited or was reused during sampling."
                raise ProcessCrashedError(msg + never_exec_suffix())
            if tracked_starttime is None:
                tracked_starttime = before["starttime"]

            instant = sampler.sample_tree()
            after = read_process_identity(root_pid, sampler.proc_root)
            actual_delta = t_now - last_sample_time
            last_sample_time = t_now

            identity_broke = (
                after is None
                or after["starttime"] != before["starttime"]
                or before["exe"] != after["exe"]
                or (expected_exe_real is not None and (before["exe"] is None or after["exe"] is None))
            )
            if identity_broke:
                if after is None or after["starttime"] != before["starttime"]:
                    raise ProcessCrashedError(
                        f"Process PID {root_pid} exited or was reused across a sample."
                        + never_exec_suffix()
                    )
                excluded_uncertainty += 1
                b_exe, a_exe = before["exe"], after["exe"]
                if expected_exe_real is not None and a_exe == expected_exe_real and b_exe not in (None, expected_exe_real):
                    if pre_exec_gate and exec_discovered_monotonic is None:
                        exec_discovered_monotonic = time.monotonic()
                    target_seen = True
                elif (
                    expected_exe_real is not None
                    and a_exe is not None
                    and b_exe is not None
                    and a_exe != b_exe
                    and a_exe != expected_exe_real
                ):
                    raise HarnessError(
                        f"Unexpected executable {a_exe!r}; expected {expected_exe_real!r}."
                    )
                if ready_monotonic is None and t_now - start_monotonic > readiness_timeout:
                    diag = monitor.get_diagnostic_log()
                    if pgrp is not None and proc is not None:
                        cleanup_process_group(pgrp, proc)
                    raise ReadinessTimeoutError(
                        f"Timed out after {readiness_timeout}s waiting for literal readiness marker '{ready_marker}'."
                        f"{never_exec_suffix()}\nDiagnostic log tail:\n{diag}"
                    )
                time.sleep(max(0.0, sample_interval - (time.monotonic() - t_now)))
                continue

            exe = before["exe"]
            if initial_exe is None and exe is not None:
                initial_exe = exe
            if (
                expected_exe_real is not None
                and exe not in (None, expected_exe_real, initial_exe, gate_exe)
            ):
                raise HarnessError(
                    f"Unexpected executable {exe!r}; expected {expected_exe_real!r}."
                )
            if expected_exe_real is not None and exe == expected_exe_real:
                target_seen = True
                if pre_exec_gate and exec_discovered_monotonic is None:
                    exec_discovered_monotonic = t_now

            if not instant["valid"]:
                if proc is not None and proc.poll() is not None:
                    time.sleep(0.05)
                    diag = monitor.get_diagnostic_log()
                    cleanup_process_group(pgrp, proc)
                    raise ProcessCrashedError(
                        f"Process exited prematurely during sampling with code {proc.returncode}.{never_exec_suffix()}\n{diag}"
                    )
                if proc is None and read_proc_starttime(root_pid, sampler.proc_root) != tracked_starttime:
                    raise ProcessCrashedError(
                        f"Attached process PID {root_pid} exited or was reused during sampling.{never_exec_suffix()}"
                    )
                unreadable_root_count += 1
                if unreadable_root_count > 10:
                    diag = monitor.get_diagnostic_log()
                    if pgrp is not None and proc is not None:
                        cleanup_process_group(pgrp, proc)
                    raise HarnessError(
                        f"Root process PID {root_pid} /proc is unreadable or reported zero RSS across 10 consecutive ticks.\n{diag}"
                    )
            else:
                unreadable_root_count = 0
                if marker_visible() and ready_monotonic is None:
                    if expected_exe_real is not None and exe != expected_exe_real:
                        raise HarnessError(
                            f"Readiness marker {ready_marker!r} does not certify executable {exe!r}; "
                            f"expected {expected_exe_real!r}."
                        )
                phase = _sample_phase(
                    ready=ready_monotonic is not None,
                    exe=exe,
                    expected=expected_exe_real,
                    pre_exec_gate=pre_exec_gate,
                    transition_seen=exec_discovered_monotonic is not None,
                    spawned=proc is not None,
                )
                samples.append({
                    "sampleIndex": sample_index,
                    "timestampMonotonic": t_now,
                    "elapsedSec": round(t_now - start_monotonic, 4),
                    "actualIntervalSec": round(actual_delta, 4),
                    "phase": phase,
                    "exe": exe,
                    "rootSampled": instant["rootSampled"],
                    "rootRssBytes": instant["rootRssBytes"],
                    "unreadableChildrenPids": instant["unreadableChildrenPids"],
                    "processCount": instant["processCount"],
                    "pids": instant["pids"],
                    "perProcess": instant["perProcess"],
                    "totalRssBytes": instant["totalRssBytes"],
                    "totalRssMib": round(instant["totalRssBytes"] / (1024 * 1024), 2),
                    "totalPssBytes": instant["totalPssBytes"],
                    "totalPssMib": round(instant["totalPssBytes"] / (1024 * 1024), 2) if instant["totalPssBytes"] is not None else None,
                    "pssAvailable": instant["pssAvailable"],
                    "provenance": instant["provenance"],
                })
                sample_index += 1
                if marker_visible() and ready_monotonic is None:
                    ready_monotonic = t_now

            if ready_monotonic is None:
                if t_now - start_monotonic > readiness_timeout:
                    diag = monitor.get_diagnostic_log()
                    if pgrp is not None and proc is not None:
                        cleanup_process_group(pgrp, proc)
                    raise ReadinessTimeoutError(
                        f"Timed out after {readiness_timeout}s waiting for literal readiness marker '{ready_marker}'."
                        f"{never_exec_suffix()}\nDiagnostic log tail:\n{diag}"
                    )
            elif t_now - ready_monotonic >= steady_seconds:
                break

            time.sleep(max(0.0, sample_interval - (time.monotonic() - t_now)))

        end_monotonic = time.monotonic()
        root_vm_hwm_bytes, hwm_prov = sampler.read_root_vm_hwm()

    finally:
        # Restore original signal handlers
        signal.signal(signal.SIGTERM, original_sigterm)
        signal.signal(signal.SIGINT, original_sigint)

        # Guarantee process group cleanup on all paths
        if pgrp is not None and proc is not None:
            cleanup_process_group(pgrp, proc)

        if t_out:
            t_out.join(timeout=0.2)
        if t_err:
            t_err.join(timeout=0.2)

    if not samples:
        raise ZeroSamplesError(f"Zero valid samples collected for profile '{profile_label}'. Cannot emit successful zeros.")

    if ready_monotonic is None:
        raise ReadinessTimeoutError(f"Profile '{profile_label}' ended without reaching readiness marker.")

    if expected_exe_real is not None and not target_seen:
        raise HarnessError(
            f"Profile '{profile_label}' finished without observing expected executable '{expected_exe}'."
        )

    # Segregate samples
    steady_samples = [s for s in samples if s["phase"] == "steady"]
    launch_samples = [s for s in samples if s["phase"] == "launch"]
    pre_ready_samples = [s for s in samples if s["phase"] == "pre-ready"]
    launcher_setup_samples = [s for s in samples if s["phase"] == "launcher-setup"]
    target_samples = [s for s in samples if s["phase"] in ("launch", "pre-ready", "steady")]

    if not steady_samples:
        raise ZeroSamplesError(f"Profile '{profile_label}' collected zero valid steady-state samples.")

    # Strict validation: all steady samples must have positive RSS and root sampled
    for s in steady_samples:
        if s["totalRssBytes"] <= 0 or not s.get("rootSampled", True):
            raise ZeroSamplesError(f"Sample index {s['sampleIndex']} recorded zero or non-positive RSS.")

    # Calculate steady metrics
    steady_rss_list = [s["totalRssBytes"] for s in steady_samples]
    steady_rss_median = statistics.median(steady_rss_list)
    steady_rss_p95 = calculate_p95(steady_rss_list)
    steady_rss_max = max(steady_rss_list)

    if steady_rss_median <= 0:
        raise ZeroSamplesError(f"Profile '{profile_label}' steady-state median RSS is non-positive.")

    steady_pss_list = [s["totalPssBytes"] for s in steady_samples if s["totalPssBytes"] is not None]
    if len(steady_pss_list) == len(steady_samples):
        steady_pss_median: float | None = float(statistics.median(steady_pss_list))
        steady_pss_p95: float | None = float(calculate_p95(steady_pss_list))
        steady_pss_max: float | None = float(max(steady_pss_list))
    else:
        steady_pss_median = None
        steady_pss_p95 = None
        steady_pss_max = None

    if expected_exe_real is not None:
        for s in steady_samples + launch_samples + pre_ready_samples:
            if s.get("exe") != expected_exe_real:
                raise HarnessError(
                    f"Sample {s['sampleIndex']} phase {s['phase']} exe {s.get('exe')!r} is not expected {expected_exe_real!r}."
                )

    if pre_exec_gate and exec_discovered_monotonic is not None:
        launch_claim: str | None = "verified-pre-exec-gate"
    elif proc is not None and expected_exe_real is None:
        launch_claim = "spawned-by-harness"
    else:
        launch_claim = None
    has_launch_phase = launch_claim is not None and len(launch_samples) > 0
    launch_rss_list = [s["totalRssBytes"] for s in launch_samples]
    launch_peak_rss = max(launch_rss_list) if launch_rss_list else None
    launch_pss_list = [s["totalPssBytes"] for s in launch_samples if s["totalPssBytes"] is not None]
    launch_peak_pss = max(launch_pss_list) if (launch_pss_list and len(launch_pss_list) == len(launch_samples)) else None

    # Overall peaks across target application lifetime (strictly excluding launcher-setup!)
    all_rss_list = [s["totalRssBytes"] for s in target_samples]
    sampled_peak_rss = max(all_rss_list) if all_rss_list else 0

    all_pss_list = [s["totalPssBytes"] for s in target_samples if s["totalPssBytes"] is not None]
    sampled_peak_pss = max(all_pss_list) if len(all_pss_list) == len(target_samples) else None

    # Actual interval stats
    intervals = [s["actualIntervalSec"] for s in samples[1:]] if len(samples) > 1 else [sample_interval]
    actual_mean_interval = statistics.mean(intervals)
    actual_median_interval = statistics.median(intervals)
    actual_p95_interval = calculate_p95(intervals)
    actual_min_interval = min(intervals)
    actual_max_interval = max(intervals)

    process_counts = [s["processCount"] for s in samples]
    max_proc_count = max(process_counts)
    steady_proc_median = int(statistics.median([s["processCount"] for s in steady_samples]))

    return {
        "profileLabel": profile_label,
        "workloadRevision": workload_revision,
        "harnessRevision": HARNESS_REVISION,
        "argv": effective_argv,
        "readyMarker": ready_marker,
        "status": "COMPLETED",
        "mode": "spawn" if attach_pid is None else "attach",
        "attachedStarttimeTicks": attach_starttime if attach_pid is not None else None,
        "timestamps": {
            "startIsoUtc": start_iso,
            "startMonotonic": round(start_monotonic, 4),
            "execDiscoveredMonotonic": round(exec_discovered_monotonic, 4) if exec_discovered_monotonic is not None else None,
            "readyMonotonic": round(ready_monotonic, 4),
            "endMonotonic": round(end_monotonic, 4),
            "deadlineMonotonic": round(deadline_monotonic, 4),
            "launchDurationSec": round(ready_monotonic - (exec_discovered_monotonic or start_monotonic), 4) if has_launch_phase else None,
            "attachToReadySec": round(ready_monotonic - start_monotonic, 4),
            "steadyDurationSec": round(end_monotonic - ready_monotonic, 4),
            "totalDurationSec": round(end_monotonic - start_monotonic, 4),
        },
        "sampleInterval": {
            "targetSec": sample_interval,
            "actualSampleCount": len(samples),
            "steadySampleCount": len(steady_samples),
            "launchSampleCount": len(launch_samples),
            "preReadySampleCount": len(pre_ready_samples),
            "launcherSetupSampleCount": len(launcher_setup_samples),
            "excludedUncertaintyCount": excluded_uncertainty,
            "actualMeanSec": round(actual_mean_interval, 5),
            "actualMedianSec": round(actual_median_interval, 5),
            "actualP95Sec": round(actual_p95_interval, 5),
            "actualMinSec": round(actual_min_interval, 5),
            "actualMaxSec": round(actual_max_interval, 5),
        },
        "launchMetrics": {
            "sampledPeakRssBytes": int(launch_peak_rss) if launch_peak_rss is not None else None,
            "sampledPeakRssMib": round(launch_peak_rss / (1024 * 1024), 2) if launch_peak_rss is not None else None,
            "sampledPeakPssBytes": int(launch_peak_pss) if launch_peak_pss is not None else None,
            "sampledPeakPssMib": round(launch_peak_pss / (1024 * 1024), 2) if launch_peak_pss is not None else None,
            "durationSec": round(ready_monotonic - (exec_discovered_monotonic or start_monotonic), 4),
        } if has_launch_phase else None,
        "launchClaim": launch_claim if has_launch_phase else None,
        "samplerGate": {
            "published": gate_published,
            "exeAtPublish": gate_exe,
            "preExec": pre_exec_gate,
        } if sampler_ready_path else None,
        "steadyMetrics": {
            "rssMedianBytes": int(steady_rss_median),
            "rssMedianMib": round(steady_rss_median / (1024 * 1024), 2),
            "rssP95Bytes": int(steady_rss_p95),
            "rssP95Mib": round(steady_rss_p95 / (1024 * 1024), 2),
            "rssMaxBytes": int(steady_rss_max),
            "rssMaxMib": round(steady_rss_max / (1024 * 1024), 2),
            "steadySampledPeakRssBytes": int(steady_rss_max),
            "steadySampledPeakRssMib": round(steady_rss_max / (1024 * 1024), 2),
            "pssMedianBytes": int(steady_pss_median) if steady_pss_median is not None else None,
            "pssMedianMib": round(steady_pss_median / (1024 * 1024), 2) if steady_pss_median is not None else None,
            "pssP95Bytes": int(steady_pss_p95) if steady_pss_p95 is not None else None,
            "pssP95Mib": round(steady_pss_p95 / (1024 * 1024), 2) if steady_pss_p95 is not None else None,
            "pssMaxBytes": int(steady_pss_max) if steady_pss_max is not None else None,
            "pssMaxMib": round(steady_pss_max / (1024 * 1024), 2) if steady_pss_max is not None else None,
            "pssAvailable": steady_pss_median is not None,
        },
        "overallPeak": {
            "sampledPeakRssBytes": int(sampled_peak_rss),
            "sampledPeakRssMib": round(sampled_peak_rss / (1024 * 1024), 2),
            "sampledPeakPssBytes": int(sampled_peak_pss) if sampled_peak_pss is not None else None,
            "sampledPeakPssMib": round(sampled_peak_pss / (1024 * 1024), 2) if sampled_peak_pss is not None else None,
        },
        "mainProcessVmHwm": {
            "bytes": root_vm_hwm_bytes,
            "mib": round(root_vm_hwm_bytes / (1024 * 1024), 2) if root_vm_hwm_bytes is not None else None,
            "provenance": hwm_prov,
        },
        "processTree": {
            "rootPid": root_pid,
            "maxProcessCount": max_proc_count,
            "steadyMedianProcessCount": steady_proc_median,
        },
        "counterProvenance": {
            "rss": "smaps_rollup (fallback to statm)",
            "pss": "smaps_rollup (strict null if smaps_rollup unavailable; never fallback to statm)",
            "hwm": "proc_status_vmhwm (root process only; children HWMs are strictly never summed)",
        },
        "limitations": {
            "sampledPeaks": "Discrete sampling (~50ms) is a sampled peak, not a continuous hardware peak, and does not observe the first dynamic-linker instruction. launcher-setup samples and ticks whose exe changed or was unreadable are excluded from target peaks.",
            "launchPhase": "Claimed only after a sampler-ready gate published while /proc/<pid>/exe was not the expected binary, plus a later tick that saw that exe. A matching exe at attach is pre-ready, not launch.",
            "gpuMemory": "GPU allocations, hardware textures, and display-server shared buffers outside the user-space Linux process tree are not accounted in RSS/PSS.",
        },
        "samples": samples,
    }


def generate_markdown_report(report_data: dict[str, Any]) -> str:
    """Formats benchmark results into a clean Markdown table and provenance narrative."""
    env = report_data.get("environment", {})
    results = report_data.get("results", [])

    lines: list[str] = [
        "# snip-sync 記憶體量測基準報告 (Memory Baseline Report)",
        "",
        f"日期：{report_data.get('timestampUtc', 'N/A')}",
        "測試環境：",
        f"- 作業系統：{env.get('os', 'Linux')} ({env.get('kernel', '')})",
        f"- CPU：{env.get('cpu', 'N/A')}",
        f"- 實體記憶體：{env.get('memoryTotalMib', 'N/A')} MiB",
        f"- 顯示伺服器：{env.get('display', 'none')}",
        f"- 測試工作區：{report_data.get('workspaceDir', 'N/A')}",
        f"- 基準套件版本：Harness {report_data.get('harnessRevision', HARNESS_REVISION)} / Workload {report_data.get('workloadRevision', 'N/A')}",
        "",
        "## 量測結果總表（程序樹連續取樣，區分穩態與峰值）",
        "",
        "| 方案與測試階段 | 狀態 | 穩態程序數 | 穩態 RSS (MiB) | 穩態 PSS (MiB) | 取樣並行峰值 RSS (MiB) | 主程序 VmHWM (MiB) | 備註 |",
        "| --- | :---: | :---: | :---: | :---: | :---: | :---: | --- |",
    ]

    short_sample_warnings: list[str] = []

    for r in results:
        label = r.get("profileLabel", "Unknown")
        status = r.get("status", "UNKNOWN")
        if status == "COMPLETED":
            procs = r.get("processTree", {}).get("steadyMedianProcessCount", 1)
            steady = r.get("steadyMetrics", {})
            s_rss = f"{steady.get('rssMedianMib', 0.0):.2f}"
            s_pss = f"{steady.get('pssMedianMib'):.2f}" if steady.get("pssMedianMib") is not None else "N/A"
            p_rss = f"{r.get('overallPeak', {}).get('sampledPeakRssMib', 0.0):.2f}"
            hwm = f"{r.get('mainProcessVmHwm', {}).get('mib'):.2f}" if r.get('mainProcessVmHwm', {}).get('mib') is not None else "N/A"

            steady_dur = r.get("timestamps", {}).get("steadyDurationSec", 0.0)
            sample_cnt = r.get("sampleInterval", {}).get("steadySampleCount", 0)

            if steady_dur < 30.0:
                note = f"⚠️ 短採樣 {steady_dur:.1f}s ({sample_cnt} 樣本)"
                short_sample_warnings.append(
                    f"- **{label}**：實測穩態僅 {steady_dur:.1f} 秒（未達發布標準門檻 ≥30 秒），屬觀察性短取樣。"
                )
            else:
                note = f"穩態 {steady_dur:.1f}s ({sample_cnt} 樣本)"

            lines.append(f"| {label} | ✓ 完成 | {procs} | {s_rss} | {s_pss} | {p_rss} | {hwm} | {note} |")
        elif status == "PENDING":
            reason = r.get("reason", "等待自動化驅動")
            lines.append(f"| {label} | ⏳ 待實作 | - | - | - | - | - | {reason} |")
        elif status == "UNAVAILABLE":
            reason = r.get("reason", "無法等價比較")
            lines.append(f"| {label} | ✕ 無對應 | - | - | - | - | - | {reason} |")
        else:
            lines.append(f"| {label} | ✕ 失敗 | - | - | - | - | - | {r.get('error', '未知錯誤')} |")

    if short_sample_warnings:
        lines.extend([
            "",
            "> [!WARNING]",
            "> **觀察性短取樣提示 (Observational Short Sample Notice)**：",
            *short_sample_warnings,
            "> 觀察性採樣數據不可作為正式切換之最終發布門檻依據。",
        ])

    lines.extend([
        "",
        "## 量測口徑與計價邊界說明",
        "",
        "1. **程序樹連續取樣 (Continuous Process Tree Sampling)**：",
        "   - 使用 Python stdlib 直接讀取 `/proc/<pid>/smaps_rollup`，並以 `/proc/<pid>/task/<tid>/children` 及 process group 完整鎖定衍生子程序。",
        "   - 採集過程消除 shell 頻繁 fork `pgrep`、`grep`、`awk` 造成的抖動，採樣間隔如實記錄實際單調時鐘（nominal ~50ms）。",
        "2. **計數器真實來源與單位 (Counter Provenance)**：",
        "   - **RSS**：優先讀取 `smaps_rollup` 之 `Rss`，若無則降級為 `statm` resident pages × page size。",
        "   - **PSS**：嚴格取自 `smaps_rollup` 之 `Pss`。若核心未啟用或無權限讀取，數值標記為 `null` (N/A)，**絕對不拿 RSS 混充 PSS**。",
        "   - **主程序 VmHWM**：取自 Linux 核心 `/proc/<root_pid>/status` 之 `VmHWM`，誠實反映主程序生命週期最高水位。**嚴禁加總子程序之 VmHWM**（生命週期峰值相加在數學與系統語意上均不成立）。",
        "3. **就緒標記 (Readiness Markers)**：",
        "   - 採滾動緩衝區精確字面比對（Literal Substring Match），跨 chunk 邊界亦能精確識別，杜絕正則 `grep [READY:...]` 誤匹配單一字元之缺陷。",
        "   - 測試座親自 spawn 且未指定預期執行檔時，就緒前為 launch。Attach 預設為 pre-ready。只有在取樣閘道發布時 exe 仍不是預期目標、且後續觀察到該 exe 轉換時，就緒前才標為 launch。launcher-setup 不計入目標峰值。就緒後為穩態（發布門檻 ≥30 秒），以中位數與 p95 判定，而非最後一筆樣本。",
        "4. **量測限制揭露 (Measurement Limitations)**：",
        "   - **瞬態子程序 (Transient Children)**：受限於 ~50ms 離散週期取樣，生命週期小於取樣週期的瞬態 Git 子程序可能未被取樣點捕捉；取樣峰值代表取樣時點觀察到之並行程序樹記憶體峰值。",
        "   - **GPU 與顯示緩衝區**：Linux `/proc` RSS/PSS 僅記錄使用者空間虛擬記憶體常駐分頁，GPU 專屬配置、DMA 緩衝區與 X11/Wayland 合成器表面不計入此數字。",
        "5. **Profiles 狀態**：",
        "   - **100 次連續切換 (100-switch Soak)**：標記為 `PENDING`。目前 Native CLI 尚未提供原生 UI 驅動切換參數，不進行虛假插樁。",
        "   - **Tauri Baseline 對照**：標記為 `UNAVAILABLE`。現階段 Tauri 版本無等價之無周邊 overview/preview 命令行量測入口，在未具備相同工作負載與建置條件前不宣稱節省比例。",
    ])

    return "\n".join(lines) + "\n"


def run_standard_suite(
    bin_path: str,
    workspace_dir: str,
    out_dir: str,
    steady_seconds: float = 30.0,
    sample_interval: float = 0.050,
    workload_revision: str = HARNESS_REVISION,
) -> dict[str, Any]:
    """
    Executes the standard suite of workbench profiles:
    - GPUI Release (Idle/Empty)
    - GPUI Release (1 Repo Overview)
    - GPUI Release (15 Repos Overview)
    - GPUI Release (15 Repos Preview)
    And appends pending/unavailable placeholders for soak and Tauri.
    """
    os.makedirs(out_dir, exist_ok=True)

    # Prepare empty directory for idle
    empty_temp = tempfile.mkdtemp(prefix="snip-empty-workspace-")

    # Locate one repo directory within workspace
    first_repo_dir = workspace_dir
    if os.path.isdir(workspace_dir):
        subdirs = [os.path.join(workspace_dir, d) for d in os.listdir(workspace_dir) if os.path.isdir(os.path.join(workspace_dir, d))]
        if subdirs:
            first_repo_dir = sorted(subdirs)[0]

    profiles = [
        {
            "label": "GPUI Release (Idle/Empty)",
            "argv": [bin_path, "--workspace", empty_temp, "--mode", "idle"],
            "marker": "[READY:IDLE]",
            "steady": steady_seconds,
        },
        {
            "label": "GPUI Release (1 Repo Overview)",
            "argv": [bin_path, "--workspace", first_repo_dir, "--mode", "overview"],
            "marker": "[READY:OVERVIEW]",
            "steady": steady_seconds,
        },
        {
            "label": "GPUI Release (15 Repos Overview)",
            "argv": [bin_path, "--workspace", workspace_dir, "--mode", "overview"],
            "marker": "[READY:OVERVIEW]",
            "steady": steady_seconds,
        },
        {
            "label": "GPUI Release (15 Repos Preview)",
            "argv": [bin_path, "--workspace", workspace_dir, "--mode", "preview"],
            "marker": "[READY:PREVIEW]",
            "steady": steady_seconds,
        },
    ]

    suite_results: list[dict[str, Any]] = []
    all_raw_samples: list[dict[str, Any]] = []

    try:
        for p_cfg in profiles:
            label = p_cfg["label"]
            print(f"Executing profile: {label}...")
            print(f"  Command: {p_cfg['argv']}")
            print(f"  Readiness marker: {p_cfg['marker']}")
            try:
                res = measure_single_profile(
                    argv=p_cfg["argv"],
                    ready_marker=p_cfg["marker"],
                    profile_label=label,
                    steady_seconds=p_cfg["steady"],
                    sample_interval=sample_interval,
                    workload_revision=workload_revision,
                )
                samples = res.pop("samples")
                for s in samples:
                    s["profileLabel"] = label
                    all_raw_samples.append(s)

                suite_results.append(res)
                steady = res["steadyMetrics"]
                pss_str = f"{steady['pssMedianMib']:.2f} MiB" if steady['pssMedianMib'] is not None else "N/A"
                print(f"  -> Procs: {res['processTree']['steadyMedianProcessCount']} | Steady RSS: {steady['rssMedianMib']:.2f} MiB | Steady PSS: {pss_str} | Peak RSS: {res['overallPeak']['sampledPeakRssMib']:.2f} MiB | Main VmHWM: {res['mainProcessVmHwm']['mib']} MiB")
            except Exception as e:
                print(f"  -> Profile failed: {e}", file=sys.stderr)
                suite_results.append({
                    "profileLabel": label,
                    "status": "FAILED",
                    "error": str(e),
                })
                raise

        # Soak test profile (Pending UI driver)
        suite_results.append({
            "profileLabel": "100-Switch Soak Test (Repo/Preview Churn)",
            "status": "PENDING",
            "reason": "CLI lacks automated 100-switch input driver; pending native UI test automation driver rather than faked metric.",
        })

        # Tauri comparable profile (Unavailable without equivalent CLI headless mode)
        suite_results.append({
            "profileLabel": "Tauri Baseline (Comparable Overview/Preview)",
            "status": "UNAVAILABLE",
            "reason": "No comparable headless/CLI overview-preview profile currently exists for Tauri desktop app; no claimed savings without identical workload.",
        })

    finally:
        shutil.rmtree(empty_temp, ignore_errors=True)

    # Write raw JSONL samples
    jsonl_path = os.path.join(out_dir, "raw_samples.jsonl")
    with open(jsonl_path, "w", encoding="utf-8") as f:
        for s in all_raw_samples:
            f.write(json.dumps(s) + "\n")

    report_payload = {
        "timestampUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "harnessRevision": HARNESS_REVISION,
        "workloadRevision": workload_revision,
        "workspaceDir": workspace_dir,
        "environment": get_system_environment(),
        "results": suite_results,
    }

    # Write JSON report
    json_path = os.path.join(out_dir, "benchmark_report.json")
    with open(json_path, "w", encoding="utf-8") as f:
        json.dump(report_payload, f, indent=2)

    # Write Markdown report
    md_path = os.path.join(out_dir, "benchmark_report.md")
    md_content = generate_markdown_report(report_payload)
    with open(md_path, "w", encoding="utf-8") as f:
        f.write(md_content)

    print(f"\nMeasurement complete! Output written to:\n  {json_path}\n  {jsonl_path}\n  {md_path}")
    return report_payload


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Linux Memory & Footprint Benchmark Harness for snip-sync."
    )
    parser.add_argument(
        "--suite",
        action="store_true",
        help="Run standard benchmark suite (idle, 1-repo overview, 15-repo overview, 15-repo preview).",
    )
    parser.add_argument(
        "--workspace",
        default=None,
        help="Workspace directory containing repositories (required for --suite).",
    )
    parser.add_argument(
        "--bin",
        default=None,
        help="Path to executable binary under test.",
    )
    parser.add_argument(
        "--bin-args",
        nargs="*",
        default=[],
        help="Explicit arguments passed to the executable binary.",
    )
    parser.add_argument(
        "--cmd-json",
        default=None,
        help="Explicit argv JSON array (e.g. '[\"/path/bin\", \"--arg1\"]').",
    )
    parser.add_argument(
        "--ready-marker",
        default="[READY:IDLE]",
        help="Literal string required for readiness marker (default: [READY:IDLE]).",
    )
    parser.add_argument(
        "--profile-label",
        default="Custom Benchmark Run",
        help="Human-readable label for this benchmark profile.",
    )
    parser.add_argument(
        "--steady-seconds",
        type=float,
        default=30.0,
        help="Duration in seconds to sample in steady state after ready marker (default: 30.0).",
    )
    parser.add_argument(
        "--sample-interval",
        type=float,
        default=0.050,
        help="Target sample interval in seconds (default: 0.050s = 50ms).",
    )
    parser.add_argument(
        "--readiness-timeout",
        type=float,
        default=15.0,
        help="Maximum seconds to wait for readiness marker before failing (default: 15.0).",
    )
    parser.add_argument(
        "--out-dir",
        default="target/benchmark-results",
        help="Output directory for benchmark reports and JSONL (default: target/benchmark-results).",
    )
    parser.add_argument(
        "--workload-revision",
        default=HARNESS_REVISION,
        help=f"Workload revision identifier (default: {HARNESS_REVISION}).",
    )
    parser.add_argument(
        "--attach-pid",
        type=int,
        default=None,
        help="Attach to an already running root PID instead of spawning a new process.",
    )
    parser.add_argument(
        "--ready-file",
        default=None,
        help="Path to a readiness signal file monitored for the ready marker (must not exist yet).",
    )
    parser.add_argument(
        "--attach-starttime",
        type=int,
        default=None,
        help="Expected /proc starttime (clock ticks) of --attach-pid; a mismatch means PID reuse and fails.",
    )
    parser.add_argument(
        "--sampler-ready-file",
        default=None,
        help="Path to write when the sampler is attached and ready to sample, before the target executes.",
    )
    parser.add_argument(
        "--expected-exe",
        default=None,
        help="Path to expected binary after execv (verifies launcher transition and excludes launcher setup samples).",
    )
    parser.add_argument(
        "rest_argv",
        nargs="*",
        help="Trailing arguments after -- treated as argv for single profile run.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    raw_argv = argv if argv is not None else sys.argv[1:]
    trailing_argv: list[str] = []
    if "--" in raw_argv:
        sep_idx = raw_argv.index("--")
        harness_argv = raw_argv[:sep_idx]
        trailing_argv = raw_argv[sep_idx + 1:]
    else:
        harness_argv = raw_argv

    args = parse_args(harness_argv)

    out_dir = os.path.abspath(args.out_dir)
    os.makedirs(out_dir, exist_ok=True)

    if args.suite:
        if not args.bin:
            print("Error: --bin <PATH> is required when running --suite.", file=sys.stderr)
            return 1
        if not args.workspace:
            print("Error: --workspace <PATH> is required when running --suite.", file=sys.stderr)
            return 1
        try:
            run_standard_suite(
                bin_path=os.path.abspath(args.bin),
                workspace_dir=os.path.abspath(args.workspace),
                out_dir=out_dir,
                steady_seconds=args.steady_seconds,
                sample_interval=args.sample_interval,
                workload_revision=args.workload_revision,
            )
            return 0
        except Exception as e:
            print(f"Suite error: {e}", file=sys.stderr)
            return 1

    # Single profile run
    target_argv: list[str] = []
    if trailing_argv:
        target_argv = trailing_argv
    elif args.cmd_json:
        try:
            target_argv = json.loads(args.cmd_json)
        except Exception as e:
            print(f"Error parsing --cmd-json: {e}", file=sys.stderr)
            return 1
    elif args.bin:
        target_argv = [args.bin] + args.bin_args
    elif args.rest_argv:
        target_argv = args.rest_argv
    elif args.attach_pid is not None:
        target_argv = [f"(attached-pid:{args.attach_pid})"]
    else:
        print("Error: Specify binary via trailing args after -- or --cmd-json or --bin or --attach-pid", file=sys.stderr)
        return 1

    try:
        result = measure_single_profile(
            argv=target_argv,
            ready_marker=args.ready_marker,
            profile_label=args.profile_label,
            steady_seconds=args.steady_seconds,
            sample_interval=args.sample_interval,
            readiness_timeout=args.readiness_timeout,
            workload_revision=args.workload_revision,
            attach_pid=args.attach_pid,
            ready_file=args.ready_file,
            attach_starttime=args.attach_starttime,
            sampler_ready_file=args.sampler_ready_file,
            expected_exe=args.expected_exe,
        )

        samples = result.pop("samples")
        # Write JSONL
        jsonl_path = os.path.join(out_dir, "raw_samples.jsonl")
        with open(jsonl_path, "w", encoding="utf-8") as f:
            for s in samples:
                f.write(json.dumps(s) + "\n")

        # Report payload
        report_payload = {
            "timestampUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "harnessRevision": HARNESS_REVISION,
            "workloadRevision": args.workload_revision,
            "workspaceDir": "custom_profile",
            "environment": get_system_environment(),
            "results": [result],
        }

        json_path = os.path.join(out_dir, "benchmark_report.json")
        with open(json_path, "w", encoding="utf-8") as f:
            json.dump(report_payload, f, indent=2)

        md_path = os.path.join(out_dir, "benchmark_report.md")
        with open(md_path, "w", encoding="utf-8") as f:
            f.write(generate_markdown_report(report_payload))

        steady = result["steadyMetrics"]
        pss_str = f"{steady['pssMedianMib']:.2f} MiB" if steady['pssMedianMib'] is not None else "N/A"
        print(f"Completed {args.profile_label}:")
        print(f"  Steady RSS: {steady['rssMedianMib']:.2f} MiB (p95: {steady['rssP95Mib']:.2f} MiB)")
        print(f"  Steady PSS: {pss_str}")
        print(f"  Peak RSS: {result['overallPeak']['sampledPeakRssMib']:.2f} MiB")
        print(f"  Main VmHWM: {result['mainProcessVmHwm']['mib']} MiB")
        print(f"Reports saved in: {out_dir}")
        return 0

    except Exception as e:
        print(f"Benchmark run failed: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
