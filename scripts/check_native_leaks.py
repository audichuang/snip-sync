#!/usr/bin/env python3
"""Linux native resource-leak gate (check name: native-resource-leaks).

Switches use bench_native_memory.click_repo in one round-robin cut into batches.
run_soak restarts its own index, so it is not used to count batches. Harness
teardown is not a product quit. Thresholds are initial gates and are not raised
to force a pass. Synthetic reports can exercise the math only.
"""

from __future__ import annotations

import argparse
import copy
import datetime
import hashlib
import json
import math
import os
import re
import statistics
import subprocess
import sys
import time
import uuid
from typing import Any

SCRIPTS_DIR = os.path.abspath(os.path.dirname(__file__))
REPO_ROOT = os.path.dirname(SCRIPTS_DIR)
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from bench_native_memory import (  # noqa: E402
    NativeBenchError,
    NativeSession,
    check_repo_state,
    click_repo,
    copy_explicit_selection,
    open_project_list,
    parse_bounds,
    parse_repo_select,
    repo_oracle,
    scroll_into_view,
    show_changes,
    workspace_repos,
)
from bench_tauri_memory import descendants, identity, is_same_process, load_build_receipt, sha256_file  # noqa: E402
from memory_harness import read_proc_starttime, sample_app_resources  # noqa: E402
from workload_generator import PRESETS, REPO_NAMES  # noqa: E402

CHECK_NAME = "native-resource-leaks"
SCHEMA_VERSION = 1
BATCH_SWITCHES = 10
ABS_GROWTH_BYTES = 32 * 1024 * 1024
GROWTH_FRACTION = 0.10
TREND_BYTES_PER_SWITCH = 256 * 1024
FD_GROWTH_MAX = 2
THREAD_GROWTH_MAX = 4
WATCHER_GROWTH_MAX = 0
MIN_SETTLE_SECONDS = 3.0
LONG_WINDOW_SECONDS = 30.0
LONG_OBSERVATION_SECONDS = 600.0
WINDOW_MIN_SAMPLES = 10
CANONICAL_CLIPBOARD = b"SNIP-LEAK-GATE-CANONICAL-CLIPBOARD-v1\n"
_RESOURCE_KINDS = frozenset({"settle", "settle-point", "endpoint", "terminal", "extension", "baseline", "final"})
_KNOWN_KINDS = _RESOURCE_KINDS | frozenset({"action", "interaction", "clipboard"})
_RESOURCE_KEYS = (
    "sampleId", "phase", "warmup", "activity", "resourcesComplete", "memoryComplete",
    "measuredSwitchesCompleted", "rssBytes", "pssBytes", "fdCount", "threadCount",
    "watchCount", "gitChildren", "unreadablePids", "root", "equivalentView",
    "gpuCombinedIntoRss", "vramBytes", "rssProvenance", "pssProvenance",
)

STANDARD_BINARY_CANDIDATES = (
    "target/release/snip-desktop-native",
    "target/debug/snip-desktop-native",
    "target/native-pilot/snip-desktop-native-debug-pilot",
)
RELEASE_COVERAGE = (
    "repo-switch",
    "tree",
    "history",
    "workspace-close-reopen",
    "copy",
    "paste",
    "cancel",
    "hide",
    "tray",
    "quit-cleanup",
)
SHORT_COVERAGE = (
    "repo-switch",
    "history",
    "tree",
    "copy",
    "paste",
    "cancel",
    "workspace-close-reopen",
    "quit-cleanup",
)
THRESHOLDS: dict[str, Any] = {
    "memoryGrowthAbsBytes": ABS_GROWTH_BYTES,
    "memoryGrowthFraction": GROWTH_FRACTION,
    "memoryTrendBytesPerSwitch": TREND_BYTES_PER_SWITCH,
    "fdGrowthMax": FD_GROWTH_MAX,
    "threadGrowthMax": THREAD_GROWTH_MAX,
    "watcherGrowthMax": WATCHER_GROWTH_MAX,
    "calibration": (
        "Initial regression gates, including a zero retained-watcher budget. "
        "Calibrate only from genuine stable runs. Do not raise these numbers to make a run pass. "
        "The trend is taken over RSS/PSS plus the not-yet-resident part of [heap] (see _trend_slope)."
    ),
}
FLOORS: dict[str, dict[str, Any]] = {
    "short": {
        "warmupSwitches": 20,
        "measuredSwitches": 100,
        "repos": 15,
        "checkpoints": 10,
        "settleSeconds": MIN_SETTLE_SECONDS,
    },
    "long": {
        "warmupSwitches": 20,
        "measuredSwitches": 500,
        "repos": 15,
        "checkpoints": 10,
        "settleSeconds": MIN_SETTLE_SECONDS,
        "baselineWindowSeconds": LONG_WINDOW_SECONDS,
        "finalWindowSeconds": LONG_WINDOW_SECONDS,
        "observationSeconds": LONG_OBSERVATION_SECONDS,
    },
}


class LeakError(Exception):
    pass


def _finite(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def _positive(value: Any) -> bool:
    return _finite(value) and value > 0


def _add(reasons: list[str], code: str) -> None:
    if code not in reasons:
        reasons.append(code)


def discover_standard_binary(repo_root: str) -> dict[str, Any]:
    found: list[str] = []
    checked: list[str] = []
    for rel in STANDARD_BINARY_CANDIDATES:
        path = os.path.join(repo_root, rel)
        checked.append(path)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            found.append(os.path.realpath(path))
    if len(found) == 1:
        return {"ok": True, "path": found[0], "found": found, "candidates": checked, "reason": None}
    if len(found) > 1:
        return {"ok": False, "path": None, "found": found, "candidates": checked, "reason": "multiple standard binaries"}
    return {"ok": False, "path": None, "found": [], "candidates": checked, "reason": "standard binary discovery failed"}


def source_sha_fact(repo_root: str) -> str | None:
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR")}
    try:
        out = subprocess.check_output(
            ["git", "-C", repo_root, "rev-parse", "HEAD"],
            env=env, text=True, stderr=subprocess.DEVNULL, timeout=10,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None
    return out.strip() or None


def git_readonly(repo: str, *args: str, timeout: float = 15.0) -> str:
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR")}
    return subprocess.check_output(
        ["git", "-C", repo, *args], env=env, text=True, timeout=timeout, stderr=subprocess.DEVNULL,
    )


def _standard_manifest(manifest: dict[str, Any]) -> bool:
    params = manifest.get("parameters") if isinstance(manifest.get("parameters"), dict) else {}
    summary = manifest.get("summary") if isinstance(manifest.get("summary"), dict) else {}
    repos = manifest.get("repos") if isinstance(manifest.get("repos"), list) else []
    std = PRESETS["standard"]
    if params.get("repos") != std["repos"] or params.get("filesPerRepo") != std["files"]:
        return False
    if params.get("commitsPerRepo") != std["commits"] or params.get("refsPerRepo") != std["refs"]:
        return False
    if summary.get("totalRepos") != std["repos"]:
        return False
    if summary.get("totalCommits") != std["repos"] * std["commits"]:
        return False
    if summary.get("totalTrackedPaths") != std["repos"] * std["files"]:
        return False
    if summary.get("totalRefs") != std["repos"] * std["refs"]:
        return False
    if len(repos) != std["repos"]:
        return False
    names = []
    for row in repos:
        if not isinstance(row, dict):
            return False
        names.append(row.get("name"))
        if row.get("commitCount") != std["commits"] or row.get("trackedPathsCount") != std["files"]:
            return False
        if row.get("refCount") != std["refs"]:
            return False
    return names == list(REPO_NAMES)


def classify_workload(manifest: dict[str, Any]) -> str:
    if _standard_manifest(manifest):
        return "standard-release"
    summary = manifest.get("summary") if isinstance(manifest.get("summary"), dict) else {}
    repos = manifest.get("repos") if isinstance(manifest.get("repos"), list) else []
    if summary.get("totalRepos") == 15 and len(repos) == 15:
        return "functional-15"
    return "other"


def planned_repo_sequence(names: list[str], switches: int) -> list[str]:
    if len(names) < 2:
        raise LeakError("need at least two repositories to switch")
    current: str | None = None
    chosen: list[str] = []
    for n in range(switches):
        name = names[(n + 1) % len(names)]
        if name == current:
            name = names[(n + 2) % len(names)]
        chosen.append(name)
        current = name
    return chosen


def _batches(total: int) -> list[int]:
    sizes: list[int] = []
    left = total
    while left:
        sizes.append(min(BATCH_SWITCHES, left))
        left -= sizes[-1]
    return sizes


def phase_batches(names: list[str], total: int) -> list[list[str]]:
    """One round-robin sliced into batches. A fresh run_soak call would restart at the first hop."""
    sequence = planned_repo_sequence(names, total)
    offset = 0
    pieces: list[list[str]] = []
    for count in _batches(total):
        pieces.append(sequence[offset:offset + count])
        offset += count
    return pieces


def _growth_limit(baseline: float) -> float:
    return max(ABS_GROWTH_BYTES, GROWTH_FRACTION * baseline)


def _slope(xs: list[float], ys: list[float]) -> float | None:
    if len(xs) < 2 or len(xs) != len(ys):
        return None
    x_mean = sum(xs) / len(xs)
    y_mean = sum(ys) / len(ys)
    var = sum((x - x_mean) ** 2 for x in xs)
    if var == 0:
        return None
    cov = sum((x - x_mean) * (y - y_mean) for x, y in zip(xs, ys))
    return cov / var


def _median(values: list[float]) -> float:
    return float(statistics.median(values))


def _root_key(root: Any) -> tuple[Any, Any, Any] | None:
    if not isinstance(root, dict):
        return None
    exe = root.get("exe")
    pid = root.get("pid")
    start = root.get("starttime")
    if not isinstance(exe, str) or exe.endswith(" (deleted)"):
        return None
    if isinstance(pid, bool) or isinstance(start, bool) or pid is None or start is None:
        return None
    try:
        real = os.path.realpath(exe)
    except OSError:
        real = exe
    return (pid, start, real)


def _expected_exe(report: dict[str, Any]) -> str | None:
    binary = report.get("binary")
    if not isinstance(binary, dict):
        return None
    path = binary.get("path")
    if not isinstance(path, str) or path.endswith(" (deleted)"):
        return None
    try:
        return os.path.realpath(path)
    except OSError:
        return path


def _same_root(row_root: Any, expected: tuple[Any, Any, Any] | None) -> bool:
    key = _root_key(row_root)
    return key is not None and expected is not None and key == expected


def _action_fields_ok(row: Any) -> bool:
    if not isinstance(row, dict) or row.get("input") != "click":
        return False
    if not _positive(row.get("clickToRepoLoadedMs")) or not _positive(row.get("clickToGraphLoadedMs")):
        return False
    select = row.get("selectLine") if isinstance(row.get("selectLine"), str) else ""
    loaded = row.get("loadedLine") if isinstance(row.get("loadedLine"), str) else ""
    graph = row.get("graphLine") if isinstance(row.get("graphLine"), str) else ""
    try:
        _idx, name = parse_repo_select(select)
    except NativeBenchError:
        return False
    if name != row.get("repo"):
        return False
    match = re.search(rf"\[APP:REPO_LOADED: {re.escape(name)} files=(\d+)\]", loaded)
    if match is None or "[APP:GRAPH_LOADED:" not in graph:
        return False
    return True


def _endpoint_ok(sample: Any, settle_floor: float) -> bool:
    if not isinstance(sample, dict):
        return False
    view = sample.get("viewEvidence") if isinstance(sample.get("viewEvidence"), dict) else {}
    tab = view.get("tabLine") if isinstance(view.get("tabLine"), str) else ""
    loaded = view.get("loadedLine") if isinstance(view.get("loadedLine"), str) else ""
    graph = view.get("graphLine") if isinstance(view.get("graphLine"), str) else ""
    select = view.get("selectLine") if isinstance(view.get("selectLine"), str) else ""
    try:
        _idx, name = parse_repo_select(select)
    except NativeBenchError:
        return False
    equiv = sample.get("equivalentView") if isinstance(sample.get("equivalentView"), dict) else {}
    return (
        sample.get("warmup") is False
        and sample.get("activity") == "quiescent"
        and sample.get("resourcesComplete") is True
        and sample.get("memoryComplete") is True
        and sample.get("gitChildren") == 0
        and not sample.get("unreadablePids")
        and _positive(sample.get("rssBytes"))
        and _positive(sample.get("pssBytes"))
        and _finite(sample.get("fdCount"))
        and sample.get("fdCount") >= 0
        and _finite(sample.get("threadCount"))
        and sample.get("threadCount") >= 1
        and isinstance(sample.get("watchCount"), int)
        and not isinstance(sample.get("watchCount"), bool)
        and sample.get("watchCount") >= 0
        and _finite(sample.get("settleSeconds"))
        and sample.get("settleSeconds") + 1e-9 >= settle_floor
        and _positive(sample.get("measuredSwitchesCompleted"))
        and name == equiv.get("repo")
        and f"[APP:REPO_LOADED: {name} files=" in loaded
        and "[APP:GRAPH_LOADED:" in graph
        and "TAB_SWITCHED: GitChanges" in tab
        and "visible=true" in tab
        and sample.get("gpuCombinedIntoRss") is False
        and sample.get("vramBytes") is None
        and _root_key(sample.get("root")) is not None
    )


def _has_non_finite(sample: Any) -> bool:
    if not isinstance(sample, dict):
        return False
    for key in ("rssBytes", "pssBytes", "fdCount", "threadCount", "watchCount", "settleSeconds"):
        value = sample.get(key)
        if isinstance(value, float) and not math.isfinite(value):
            return True
    return False


def _measured_endpoints(samples: list[Any]) -> list[tuple[int, dict[str, Any], list[dict[str, Any]]]]:
    groups: dict[int, list[dict[str, Any]]] = {}
    order: list[int] = []
    for sample in samples:
        if not isinstance(sample, dict) or sample.get("warmup") is True or sample.get("phase") != "measured":
            continue
        key = sample.get("batchIndex")
        if not isinstance(key, int) or isinstance(key, bool):
            continue
        if key not in groups:
            order.append(key)
            groups[key] = []
        groups[key].append(sample)
    return [(key, groups[key][-1], groups[key]) for key in order]


def _series_growth(samples: list[dict[str, Any]], key: str) -> dict[str, Any]:
    ordered = sorted(samples, key=lambda row: row["measuredSwitchesCompleted"])
    xs = [float(row["measuredSwitchesCompleted"]) for row in ordered]
    ys = [float(row[key]) for row in ordered]
    third = len(ordered) // 3
    first_med = _median(ys[:third])
    last_med = _median(ys[-third:])
    return {
        "firstThirdMedian": first_med,
        "lastThirdMedian": last_med,
        "growth": last_med - first_med,
        "growthLimit": _growth_limit(first_med) if key in ("rssBytes", "pssBytes") else None,
        "slopePerSwitch": _slope(xs, ys),
    }


def _heap_untouched(report: dict[str, Any]) -> dict[str, float]:
    """Per-sample `[heap]` address space that is mapped but not yet resident."""
    out: dict[str, float] = {}
    for row in report.get("heapReserve") or []:
        if not isinstance(row, dict) or not isinstance(row.get("sampleId"), str):
            continue
        size, rss = row.get("heapVmaBytes"), row.get("heapRssBytes")
        if _finite(size) and _finite(rss) and 0 <= rss <= size:
            out[row["sampleId"]] = float(size - rss)
    return out


def _trend_slope(samples: list[dict[str, Any]], key: str, untouched: dict[str, float]) -> float | None:
    """Slope of memory plus not-yet-resident main heap.

    Lavapipe/LLVM leave ~24 MiB of `[heap]` mapped but never written at startup. Later
    allocations reuse it, so RSS can climb late in a run with no new memory: CI runs
    36310256622 and 36313388774 both kept `[heap]` at a constant 50 MiB span and both
    ended at 49.7 MiB resident, one filling it before the measured window and one inside
    it. Adding the untouched part back keeps that flat, while heap span growth and every
    other mapping (thread arenas, mmap, GPU memfd) still count in full.
    """
    # ponytail: blind to main-arena leaks smaller than the startup heap reserve inside
    # one short run; mallinfo2 in-use bytes from the app would close that gap.
    if any(row.get("sampleId") not in untouched for row in samples):
        return None
    ordered = sorted(samples, key=lambda row: row["measuredSwitchesCompleted"])
    xs = [float(row["measuredSwitchesCompleted"]) for row in ordered]
    ys = [float(row[key]) + untouched[row["sampleId"]] for row in ordered]
    return _slope(xs, ys)


def _log_supports(item: str, row: dict[str, Any]) -> bool:
    log = row.get("log") if isinstance(row.get("log"), str) else ""
    if row.get("ok") is not True or row.get("input") not in ("click", "key"):
        return False
    if item == "tree":
        return re.search(r"\[APP:TREE_(FILE_SELECTED|EXPANDED|TOGGLED):", log) is not None
    if item == "copy":
        oracle = row.get("oracle") if isinstance(row.get("oracle"), dict) else {}
        return "[APP:COPY_DONE:" in log and oracle.get("verified") is True
    if item == "paste":
        return "[APP:PASTE_PREVIEW:" in log or "[APP:PASTE_DONE:" in log
    if item == "cancel":
        return "[APP:PASTE_CANCELLED" in log
    if item == "workspace-close-reopen":
        oracle = row.get("oracle") if isinstance(row.get("oracle"), dict) else {}
        return (
            "[APP:WORKSPACE: state=closed" in log
            and "[APP:WORKSPACE: state=open" in log
            and oracle.get("sameProcess") is True
            and oracle.get("clipboardPreserved") is True
            and oracle.get("drained") is True
            and oracle.get("reposReady") is True
        )
    if item == "quit-cleanup":
        return "[APP:QUIT:" in log
    return False


def _read_raw(path: str) -> list[dict[str, Any]] | None:
    rows: list[dict[str, Any]] = []
    try:
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    return None
                if not isinstance(obj, dict):
                    return None
                rows.append(obj)
    except OSError:
        return None
    return rows


def _action_signature(row: dict[str, Any]) -> tuple:
    oracle = row.get("oracle") if isinstance(row.get("oracle"), dict) else {}
    return (
        row.get("actionId"), row.get("phase"), row.get("repo"), row.get("input"),
        row.get("selectLine"), row.get("loadedLine"), row.get("graphLine"),
        oracle.get("ok"), oracle.get("sourceRows"), oracle.get("reportedFiles"),
        _root_key(row.get("root")),
    )


def _resource_signature(row: dict[str, Any]) -> tuple:
    equiv = row.get("equivalentView") if isinstance(row.get("equivalentView"), dict) else {}
    unread = row.get("unreadablePids")
    return (
        row.get("sampleId"), row.get("phase"), row.get("warmup"), row.get("activity"),
        row.get("resourcesComplete"), row.get("memoryComplete"),
        row.get("measuredSwitchesCompleted"), row.get("settleSeconds"), row.get("offsetSec"),
        row.get("rssBytes"), row.get("pssBytes"), row.get("fdCount"), row.get("threadCount"),
        row.get("watchCount"), row.get("gitChildren"),
        tuple(unread) if isinstance(unread, list) else unread,
        _root_key(row.get("root")),
        equiv.get("repo"), equiv.get("tool"), equiv.get("workspaceOpen", None), equiv.get("closeObserved", None),
        row.get("gpuCombinedIntoRss"), row.get("vramBytes"),
        row.get("rssProvenance"), row.get("pssProvenance"), row.get("ownedTasks", None),
    )


def _interaction_signature(row: dict[str, Any]) -> tuple:
    return (row.get("item"), row.get("ok"), row.get("input"), row.get("log"), row.get("reason"))


def _clipboard_signature(row: dict[str, Any]) -> tuple:
    attr = row.get("attribution") if isinstance(row.get("attribution"), dict) else None
    attr_key = None if attr is None else (attr.get("legitimate"), attr.get("source"), attr.get("sha256"), attr.get("bytes"))
    return (row.get("bytes"), row.get("sha256"), row.get("cleared"), attr_key)


def _state_key(sample: Any) -> tuple | None:
    if not isinstance(sample, dict):
        return None
    equiv = sample.get("equivalentView")
    if not isinstance(equiv, dict) or not isinstance(equiv.get("repo"), str) or not isinstance(equiv.get("tool"), str):
        return None
    return (equiv.get("repo"), equiv.get("tool"), equiv.get("workspaceOpen", None), equiv.get("closeObserved", None))


def _parse_raw_stream(path: str, run_id: Any) -> list[dict[str, Any]] | None:
    rows = _read_raw(path)
    if not rows or not isinstance(run_id, str) or not run_id:
        return None
    seen_sample: set[str] = set()
    seen_action: set[str] = set()
    prev_t: float | None = None
    for index, row in enumerate(rows):
        if row.get("runId") != run_id or row.get("kind") not in _KNOWN_KINDS:
            return None
        seq = row.get("seq")
        if isinstance(seq, bool) or not isinstance(seq, int) or seq != index:
            return None
        tmono = row.get("tMono")
        if not _finite(tmono):
            return None
        if prev_t is not None and tmono < prev_t:
            return None
        prev_t = float(tmono)
        kind = row["kind"]
        if kind == "action":
            action_id = row.get("actionId")
            if (
                not isinstance(action_id, str)
                or action_id in seen_action
                or row.get("phase") not in ("warmup", "measured")
                or not _action_fields_ok(row)
                or not isinstance(row.get("root"), dict)
                or not isinstance(row["root"].get("exe"), str)
                or row["root"].get("pid") is None
                or row["root"].get("starttime") is None
            ):
                return None
            seen_action.add(action_id)
        elif kind in _RESOURCE_KINDS:
            if any(key not in row for key in _RESOURCE_KEYS):
                return None
            sample_id = row.get("sampleId")
            if not isinstance(sample_id, str) or sample_id in seen_sample or not isinstance(row.get("unreadablePids"), list):
                return None
            if kind in ("baseline", "final") and "offsetSec" not in row:
                return None
            if kind in ("endpoint", "terminal") and "settleSeconds" not in row:
                return None
            seen_sample.add(sample_id)
        elif kind == "interaction":
            if not isinstance(row.get("item"), str) or not isinstance(row.get("ok"), bool):
                return None
        elif kind == "clipboard":
            if row.get("phase") not in ("baseline", "terminal"):
                return None
            if isinstance(row.get("bytes"), bool) or not isinstance(row.get("bytes"), int):
                return None
            if not isinstance(row.get("sha256"), str) or not isinstance(row.get("cleared"), bool):
                return None
    return rows


def _window_seconds(rows: list[dict[str, Any]]) -> float:
    offsets = [row.get("offsetSec") for row in rows]
    if not offsets or not all(_finite(item) for item in offsets):
        return 0.0
    return float(max(offsets))


def _dict_rows(value: Any) -> list[dict[str, Any]]:
    return [row for row in value if isinstance(row, dict)] if isinstance(value, list) else []


def _canonical_measurement(report: dict[str, Any]) -> tuple[dict[str, Any], list[str]]:
    """Project the verdict inputs. A raw JSONL stream, when present, is the only measurement source."""
    evidence = report.get("evidence") if isinstance(report.get("evidence"), dict) else {}
    measured = _dict_rows(evidence.get("measuredSwitches"))
    warmup = _dict_rows(evidence.get("warmupSwitches"))
    samples = _dict_rows(report.get("samples"))
    interactions = _dict_rows(report.get("interactions"))
    facts: dict[str, Any] = {
        "fromRaw": False,
        "measured": measured,
        "warmup": warmup,
        "samples": samples,
        "interactions": interactions,
        "windows": report.get("windows") if isinstance(report.get("windows"), dict) else {},
        "sampleOrder": report.get("sampleOrder") if isinstance(report.get("sampleOrder"), list) else [],
        "terminal": None,
        "post": [],
        "clipboard": {},
        "orderViolation": False,
    }
    raw_ref = report.get("rawSamples")
    path = raw_ref.get("path") if isinstance(raw_ref, dict) else None
    if not isinstance(path, str):
        if report.get("evidenceClass") == "product-run":
            return facts, ["raw-evidence"]
        return facts, []
    rows = _parse_raw_stream(path, report.get("runId"))
    if rows is None:
        return facts, ["raw-evidence"]
    reasons: list[str] = []
    facts["fromRaw"] = True
    raw_measured = [row for row in rows if row.get("kind") == "action" and row.get("phase") == "measured"]
    raw_warmup = [row for row in rows if row.get("kind") == "action" and row.get("phase") == "warmup"]
    raw_resources = [row for row in rows if row.get("kind") in _RESOURCE_KINDS]
    raw_interactions = [row for row in rows if row.get("kind") == "interaction"]
    if [_action_signature(row) for row in raw_measured] != [_action_signature(row) for row in measured]:
        _add(reasons, "raw-divergent")
    if [_action_signature(row) for row in raw_warmup] != [_action_signature(row) for row in warmup]:
        _add(reasons, "raw-divergent")
    checkpoint_for_compare = [row for row in raw_resources if row.get("kind") not in ("baseline", "final")]
    if [_resource_signature(row) for row in checkpoint_for_compare] != [_resource_signature(row) for row in samples]:
        _add(reasons, "raw-divergent")
    if [_interaction_signature(row) for row in raw_interactions] != [_interaction_signature(row) for row in interactions]:
        _add(reasons, "raw-divergent")
    clip_map: dict[str, dict[str, Any]] = {}
    for row in rows:
        if row.get("kind") != "clipboard":
            continue
        phase = row.get("phase")
        if phase in clip_map:
            _add(reasons, "raw-evidence")
            clip_map = {}
            break
        clip_map[str(phase)] = row
    facts["clipboard"] = clip_map
    submitted_clip = report.get("clipboard") if isinstance(report.get("clipboard"), dict) else None
    if submitted_clip is not None:
        for phase in ("baseline", "terminal"):
            raw_clip = clip_map.get(phase)
            submitted = submitted_clip.get(phase)
            if not isinstance(raw_clip, dict) or not isinstance(submitted, dict) or _clipboard_signature(raw_clip) != _clipboard_signature(submitted):
                _add(reasons, "raw-divergent")
    baseline_rows = [row for row in raw_resources if row.get("kind") == "baseline"]
    final_rows = [row for row in raw_resources if row.get("kind") == "final"]
    submitted_windows = report.get("windows") if isinstance(report.get("windows"), dict) else {}
    for name, raw_rows in (("baseline", baseline_rows), ("final", final_rows)):
        window = submitted_windows.get(name) if isinstance(submitted_windows, dict) else None
        rep_rows = _dict_rows(window.get("samples")) if isinstance(window, dict) else []
        if [_resource_signature(row) for row in raw_rows] != [_resource_signature(row) for row in rep_rows]:
            _add(reasons, "raw-divergent")
    facts["windows"] = {
        "baseline": None if not baseline_rows else {"seconds": _window_seconds(baseline_rows), "samples": baseline_rows},
        "final": None if not final_rows else {"seconds": _window_seconds(final_rows), "samples": final_rows},
    }
    checkpoint_rows = [row for row in raw_resources if row.get("kind") not in ("baseline", "final")]
    facts["measured"] = raw_measured
    facts["warmup"] = raw_warmup
    facts["samples"] = checkpoint_rows
    facts["interactions"] = raw_interactions
    facts["sampleOrder"] = [row.get("sampleId") for row in raw_resources]
    terminals = [row for row in checkpoint_rows if row.get("kind") == "terminal"]
    if len(terminals) > 1:
        _add(reasons, "raw-evidence")
    facts["terminal"] = terminals[-1] if len(terminals) == 1 else None
    endpoints = [row for row in checkpoint_rows if row.get("kind") == "endpoint"]
    last_endpoint = endpoints[-1]["seq"] if endpoints else -1
    facts["post"] = [row for row in checkpoint_rows if row["seq"] > last_endpoint and row.get("kind") in ("terminal", "extension")]
    if raw_warmup and raw_measured and max(row["seq"] for row in raw_warmup) > min(row["seq"] for row in raw_measured):
        facts["orderViolation"] = True
    for endpoint in endpoints:
        done = endpoint.get("measuredSwitchesCompleted")
        if isinstance(done, bool) or not isinstance(done, int) or done < 1 or done > len(raw_measured):
            facts["orderViolation"] = True
            continue
        if endpoint["seq"] < raw_measured[done - 1]["seq"]:
            facts["orderViolation"] = True
    if facts["terminal"] is not None:
        terminal_seq = facts["terminal"]["seq"]
        post_terminal = [row for row in rows if row["seq"] > terminal_seq]
        for row in post_terminal:
            kind = row.get("kind")
            if kind == "action":
                facts["orderViolation"] = True
            elif kind == "interaction":
                if row.get("item") != "quit-cleanup":
                    facts["orderViolation"] = True
            elif kind in ("extension", "final"):
                if report.get("profile") != "long":
                    facts["orderViolation"] = True
            else:
                facts["orderViolation"] = True
        quit_after = [row for row in post_terminal if row.get("kind") == "interaction" and row.get("item") == "quit-cleanup"]
        if len(quit_after) > 1:
            facts["orderViolation"] = True
        if final_rows and terminal_seq > min(row["seq"] for row in final_rows):
            facts["orderViolation"] = True
        if endpoints and terminal_seq < endpoints[-1]["seq"]:
            facts["orderViolation"] = True
        if raw_measured and terminal_seq < raw_measured[-1]["seq"]:
            facts["orderViolation"] = True
    if final_rows:
        start = min(row["seq"] for row in final_rows)
        end = max(row["seq"] for row in final_rows)
        final_ids = [row.get("sampleId") for row in final_rows]
        resource_ids = [row.get("sampleId") for row in raw_resources]
        for row in rows:
            if row["seq"] >= start:
                kind = row.get("kind")
                if kind == "action":
                    facts["orderViolation"] = True
                elif kind == "interaction":
                    if row["seq"] <= end or row.get("item") != "quit-cleanup":
                        facts["orderViolation"] = True
                elif row["seq"] > end:
                    facts["orderViolation"] = True
        if resource_ids[-len(final_ids):] != final_ids:
            facts["orderViolation"] = True
    if raw_resources:
        max_res = max(item["seq"] for item in raw_resources)
        for row in rows:
            if row["seq"] > max_res:
                if row.get("kind") == "action":
                    facts["orderViolation"] = True
                elif row.get("kind") == "interaction":
                    if row.get("item") != "quit-cleanup":
                        facts["orderViolation"] = True
                else:
                    facts["orderViolation"] = True
    return facts, reasons


def _clipboard_problem(facts: dict[str, Any]) -> bool:
    base = facts["clipboard"].get("baseline") if isinstance(facts.get("clipboard"), dict) else None
    end = facts["clipboard"].get("terminal") if isinstance(facts.get("clipboard"), dict) else None
    if not isinstance(base, dict) or not isinstance(end, dict):
        return True
    if base.get("cleared") is not False or end.get("cleared") is not False:
        return True
    if isinstance(base.get("bytes"), bool) or not isinstance(base.get("bytes"), int) or base["bytes"] <= 0:
        return True
    if isinstance(end.get("bytes"), bool) or not isinstance(end.get("bytes"), int) or end["bytes"] <= 0:
        return True
    if not isinstance(base.get("sha256"), str) or not isinstance(end.get("sha256"), str):
        return True
    actions = [*facts.get("warmup", []), *facts.get("measured", [])]
    if actions and isinstance(base.get("seq"), int) and base["seq"] > min(row["seq"] for row in actions if isinstance(row.get("seq"), int)):
        return True
    if base["sha256"] == end["sha256"] and base["bytes"] == end["bytes"]:
        return False
    attr = end.get("attribution") if isinstance(end.get("attribution"), dict) else None
    if (
        isinstance(attr, dict)
        and attr.get("legitimate") is True
        and attr.get("sha256") == end["sha256"]
        and attr.get("bytes") == end["bytes"]
        and isinstance(attr.get("source"), str)
        and attr["source"]
    ):
        return False
    return True


def _check_identity(report: dict[str, Any], rows: list[Any], reasons: list[str]) -> None:
    expected = _expected_exe(report)
    app = report.get("app") if isinstance(report.get("app"), dict) else None
    app_key = _root_key(app)
    if (
        expected is None
        or app_key is None
        or app_key[2] != expected
        or (isinstance(app, dict) and isinstance(app.get("exe"), str) and app["exe"].endswith(" (deleted)"))
    ):
        _add(reasons, "identity-mismatch")
    if expected and os.path.isfile(expected):
        digest = sha256_file(expected)
        pins = report.get("pins") if isinstance(report.get("pins"), dict) else {}
        pinned = pins.get("binarySha256")
        if report.get("binarySha256") not in (None, digest) or (isinstance(pinned, str) and pinned != digest):
            _add(reasons, "wrong-binary")
    after = report.get("binary") if isinstance(report.get("binary"), dict) else {}
    if report.get("evidenceClass") == "product-run":
        expected_sha = report.get("binarySha256")
        after_sha = after.get("sha256After")
        if not isinstance(expected_sha, str) or not isinstance(after_sha, str) or after_sha != expected_sha:
            _add(reasons, "wrong-binary")
    elif after.get("sha256After") not in (None, report.get("binarySha256")):
        _add(reasons, "wrong-binary")
    for row in rows:
        if not isinstance(row, dict):
            continue
        root = row.get("root")
        needs_root = bool(row.get("actionId") or row.get("sampleId") or row.get("ok") is True)
        if root is None and row.get("ok") is False and not needs_root:
            continue
        if not needs_root and not isinstance(root, dict):
            continue
        if not isinstance(root, dict):
            _add(reasons, "identity-mismatch")
            continue
        exe = root.get("exe")
        if isinstance(exe, str) and exe.endswith(" (deleted)"):
            _add(reasons, "identity-mismatch")
            continue
        key = _root_key(root)
        if key is None or (expected is not None and key[2] != expected) or (app_key is not None and key != app_key):
            _add(reasons, "identity-mismatch")


def _judge_retained(row: dict[str, Any], analysis: dict[str, Any], settle_floor: float, reasons: list[str]) -> None:
    if row.get("resourcesComplete") is not True or row.get("memoryComplete") is not True or row.get("unreadablePids"):
        _add(reasons, "incomplete-resources")
    if row.get("gitChildren") is None:
        _add(reasons, "incomplete-resources")
    elif row.get("gitChildren") != 0:
        _add(reasons, "busy-endpoint")
    if row.get("kind") == "terminal" and not _endpoint_ok(row, settle_floor):
        _add(reasons, "missing-samples")
    if not analysis:
        return
    for key, limit, code in (
        ("fdCount", FD_GROWTH_MAX, "fd-growth"),
        ("threadCount", THREAD_GROWTH_MAX, "thread-growth"),
        ("watchCount", WATCHER_GROWTH_MAX, "watcher-growth"),
    ):
        stats = analysis.get(key) if isinstance(analysis.get(key), dict) else None
        base = None if stats is None else stats.get("firstThirdMedian")
        value = row.get(key)
        if not _finite(base) or not _finite(value):
            _add(reasons, "incomplete-resources")
        elif value - base > limit:
            _add(reasons, code)
    for key in ("rssBytes", "pssBytes"):
        stats = analysis.get(key) if isinstance(analysis.get(key), dict) else None
        value = row.get(key)
        if stats and _finite(value) and _finite(stats.get("firstThirdMedian")) and value - stats["firstThirdMedian"] > stats["growthLimit"]:
            _add(reasons, "memory-growth")


def _judge_window_rows(rows: list[dict[str, Any]], reasons: list[str]) -> None:
    for row in rows:
        if row.get("resourcesComplete") is not True or row.get("memoryComplete") is not True or row.get("unreadablePids"):
            _add(reasons, "incomplete-resources")
        if not _positive(row.get("rssBytes")) or not _positive(row.get("pssBytes")):
            _add(reasons, "missing-samples")
        for key, minimum in (("fdCount", 0), ("threadCount", 1), ("watchCount", 0)):
            value = row.get(key)
            if not _finite(value) or value < minimum:
                _add(reasons, "incomplete-resources")
        if row.get("gitChildren") is None:
            _add(reasons, "incomplete-resources")
        elif row.get("gitChildren") != 0:
            _add(reasons, "busy-endpoint")
        if row.get("gpuCombinedIntoRss") is True or row.get("vramBytes") is not None:
            _add(reasons, "gpu-combined")
        if row.get("ownedTasks") == 0 or row.get("internalTasks") == 0:
            _add(reasons, "fake-tasks")


def _compare_window_counts(base_rows: list[dict[str, Any]], final_rows: list[dict[str, Any]], reasons: list[str], compare_watchers: bool) -> None:
    pairs = [("fdCount", FD_GROWTH_MAX, "fd-growth"), ("threadCount", THREAD_GROWTH_MAX, "thread-growth")]
    if compare_watchers:
        pairs.append(("watchCount", WATCHER_GROWTH_MAX, "watcher-growth"))
    for key, limit, code in pairs:
        base_values = [row.get(key) for row in base_rows]
        final_values = [row.get(key) for row in final_rows]
        if not base_values or not final_values or any(not _finite(value) or value < 0 for value in [*base_values, *final_values]):
            if base_rows or final_rows:
                _add(reasons, "incomplete-resources")
            continue
        base_med = _median([float(value) for value in base_values])
        if any(float(value) - base_med > limit for value in final_values):
            _add(reasons, code)


def _fake_task_count(report: dict[str, Any], rows: list[Any], reasons: list[str]) -> None:
    values = [report.get("ownedTasks")]
    for row in rows:
        if isinstance(row, dict):
            values.append(row.get("ownedTasks"))
            values.append(row.get("internalTasks"))
    if any(value == 0 for value in values):
        _add(reasons, "fake-tasks")


def _same_states(rows: list[Any]) -> bool:
    keys = [_state_key(row) for row in rows]
    return bool(keys) and all(key is not None and key == keys[0] for key in keys)


def _window_rows(window: Any) -> list[dict[str, Any]]:
    if not isinstance(window, dict):
        return []
    rows = window.get("samples")
    return [row for row in rows if isinstance(row, dict)] if isinstance(rows, list) else []


def _check_thresholds(report: dict[str, Any], reasons: list[str]) -> None:
    submitted = report.get("thresholds")
    if submitted is None:
        return
    if not isinstance(submitted, dict):
        _add(reasons, "thresholds-raised")
        return
    for key in (
        "memoryGrowthAbsBytes", "memoryGrowthFraction", "memoryTrendBytesPerSwitch",
        "fdGrowthMax", "threadGrowthMax", "watcherGrowthMax",
    ):
        if submitted.get(key) != THRESHOLDS[key]:
            _add(reasons, "thresholds-raised")
            return


def evaluate_report(report: dict[str, Any]) -> dict[str, Any]:
    out = copy.deepcopy(report)
    reasons: list[str] = [code for code in (out.get("reasons") or []) if isinstance(code, str)]
    profile = out.get("profile") if out.get("profile") in FLOORS else None
    if profile is None:
        _add(reasons, "bad-profile")
        profile = "short"
    floor = FLOORS[profile]
    status = out.get("driverStatus")
    if status not in ("COMPLETED", "NOT_RUN", "FAILED"):
        _add(reasons, "driver-failed")
        status = "FAILED"
    if status == "FAILED":
        _add(reasons, "driver-failed")
    _check_thresholds(out, reasons)
    out["thresholds"] = dict(THRESHOLDS)
    out["floors"] = dict(FLOORS[out["profile"]]) if out.get("profile") in FLOORS else dict(FLOORS["short"])
    out["absoluteReleaseBudgetClaimed"] = False
    if report.get("absoluteReleaseBudgetClaimed") is True:
        _add(reasons, "absolute-budget-claimed")
    out["sourceBuildAuthorized"] = False
    if report.get("sourceBuildAuthorized") is True:
        _add(reasons, "source-build-self-authorized")
    out["productAcceptance"] = False
    out["releaseComplete"] = False
    out["d4"] = "NOT_EVALUATED"
    out["schemaVersion"] = SCHEMA_VERSION
    out["check"] = CHECK_NAME

    counts = out.get("counts") if isinstance(out.get("counts"), dict) else {}
    out["counts"] = counts
    requested = counts.get("measuredSwitchesRequested")
    warmup_requested = counts.get("warmupSwitchesRequested")
    if not isinstance(requested, int) or isinstance(requested, bool) or requested < floor["measuredSwitches"]:
        _add(reasons, "undersized")
    if not isinstance(warmup_requested, int) or isinstance(warmup_requested, bool) or warmup_requested < floor["warmupSwitches"]:
        _add(reasons, "undersized")
    settle_requested = counts.get("settleSeconds")
    if not _finite(settle_requested):
        _add(reasons, "nan-sample")
    elif settle_requested < floor["settleSeconds"]:
        _add(reasons, "undersized")

    workload = out.get("workload") if isinstance(out.get("workload"), dict) else {}
    out["workload"] = workload
    if workload.get("repoCount") != floor["repos"] or workload.get("class") not in ("functional-15", "standard-release"):
        _add(reasons, "wrong-workload")
    if profile == "long" and (workload.get("class") != "standard-release" or workload.get("gitFactsMatch") is not True):
        _add(reasons, "wrong-workload")

    pins = out.get("pins") if isinstance(out.get("pins"), dict) else {}
    out["pins"] = pins
    real = out.get("evidenceClass") == "product-run" or profile == "long"
    if real or pins.get("binarySha256"):
        if not pins.get("binarySha256") or pins.get("binarySha256") != out.get("binarySha256"):
            _add(reasons, "wrong-binary")
    if real or pins.get("fixtureSha256"):
        if not pins.get("fixtureSha256") or pins.get("fixtureSha256") != out.get("fixtureSha256"):
            _add(reasons, "wrong-fixture")
    if real or pins.get("sourceSha"):
        if not pins.get("sourceSha") or pins.get("sourceSha") != out.get("sourceSha"):
            _add(reasons, "wrong-revision")

    facts, stream_reasons = _canonical_measurement(out)
    for code in stream_reasons:
        _add(reasons, code)
    if facts["orderViolation"]:
        _add(reasons, "final-window-early")
    evidence = out.get("evidence") if isinstance(out.get("evidence"), dict) else {}
    out["evidence"] = evidence
    measured = facts["measured"]
    warmup = facts["warmup"]
    evidence["measuredSwitches"] = measured
    evidence["warmupSwitches"] = warmup
    if counts.get("measuredSwitchesActual") not in (None, len(measured)):
        _add(reasons, "noop-driver")
    counts["measuredSwitchesActual"] = len(measured)
    counts["warmupSwitchesActual"] = len(warmup)
    samples = facts["samples"]
    out["samples"] = samples
    out["interactions"] = facts["interactions"]
    out["windows"] = facts["windows"]
    out["sampleOrder"] = facts["sampleOrder"]
    if facts["fromRaw"]:
        out["clipboard"] = {"baseline": facts["clipboard"].get("baseline"), "terminal": facts["clipboard"].get("terminal")}
    if any(_has_non_finite(sample) for sample in samples):
        _add(reasons, "nan-sample")

    if status == "NOT_RUN":
        out["coverage"] = {}
        out["coverageGaps"] = list(RELEASE_COVERAGE)
        out["subgates"] = {}
        out["releaseOverall"] = "NOT_ACCEPTED"
        out["verdict"] = "NOT_ACCEPTED"
        out["reasons"] = reasons
        return out

    app_root = _root_key(out.get("app"))
    if status == "COMPLETED":
        _check_identity(out, [*measured, *warmup, *samples, *facts["interactions"], *_window_rows(facts["windows"].get("baseline") if isinstance(facts["windows"], dict) else None), *_window_rows(facts["windows"].get("final") if isinstance(facts["windows"], dict) else None)], reasons)
        _fake_task_count(out, [*samples, *_window_rows(facts["windows"].get("baseline") if isinstance(facts["windows"], dict) else None), *_window_rows(facts["windows"].get("final") if isinstance(facts["windows"], dict) else None)], reasons)
        if facts["fromRaw"] and _clipboard_problem(facts):
            _add(reasons, "clipboard-state")
        if facts["fromRaw"] and profile == "short" and facts.get("terminal") is None:
            _add(reasons, "missing-samples")
        if requested != len(measured) or warmup_requested != len(warmup):
            _add(reasons, "noop-driver")
        if len(measured) < floor["measuredSwitches"] or len(warmup) < floor["warmupSwitches"]:
            _add(reasons, "undersized")
        if not measured or not all(_action_fields_ok(row) for row in measured):
            _add(reasons, "noop-driver")
        if not warmup or not all(_action_fields_ok(row) for row in warmup):
            _add(reasons, "noop-driver")
        for row in [*measured, *warmup]:
            if not isinstance(row, dict):
                _add(reasons, "repo-oracle")
                continue
            oracle = row.get("oracle") if isinstance(row.get("oracle"), dict) else {}
            loaded = row.get("loadedLine") if isinstance(row.get("loadedLine"), str) else ""
            files = re.search(r"files=(\d+)", loaded)
            reported = int(files.group(1)) if files else None
            if oracle.get("ok") is not True or oracle.get("sourceRows") != reported or oracle.get("reportedFiles") != reported:
                _add(reasons, "repo-oracle")
        names = workload.get("repos") if isinstance(workload.get("repos"), list) else []
        seen = {row.get("repo") for row in measured if isinstance(row, dict)}
        warm_seen = {row.get("repo") for row in warmup if isinstance(row, dict)}
        if set(names) != seen or len(names) != floor["repos"] or not set(names) <= warm_seen:
            _add(reasons, "repo-coverage")

    groups = _measured_endpoints(samples)
    endpoints = [endpoint for _key, endpoint, _group in groups]
    if status == "COMPLETED":
        expected_batches = len(_batches(requested)) if isinstance(requested, int) and not isinstance(requested, bool) else 0
        indexes = [key for key, _endpoint, _group in groups]
        if indexes != list(range(expected_batches)) or len(endpoints) < floor["checkpoints"]:
            _add(reasons, "undersized")
        for _key, endpoint, group in groups:
            if endpoint.get("resourcesComplete") is not True or endpoint.get("memoryComplete") is not True:
                _add(reasons, "incomplete-resources")
            if endpoint.get("gitChildren") is None or endpoint.get("unreadablePids"):
                _add(reasons, "incomplete-resources")
            elif endpoint.get("activity") != "quiescent" or endpoint.get("gitChildren") != 0:
                _add(reasons, "busy-endpoint")
            if not _endpoint_ok(endpoint, floor["settleSeconds"]):
                _add(reasons, "missing-samples")
        completions = [endpoint.get("measuredSwitchesCompleted") for endpoint in endpoints]
        if completions != sorted(completions) or len(set(completions)) != len(completions):
            _add(reasons, "missing-samples")
        elif completions and completions[-1] != len(measured):
            _add(reasons, "noop-driver")
        roots = [_root_key(endpoint.get("root")) for endpoint in endpoints]
        if not roots or any(key is None for key in roots) or len(set(roots)) != 1:
            _add(reasons, "identity-mismatch")
        elif app_root is not None and roots[0] != app_root:
            _add(reasons, "identity-mismatch")
        if endpoints:
            last_pos = max(index for index, sample in enumerate(samples) if sample is endpoints[-1] or sample == endpoints[-1])
            for sample in samples[last_pos + 1:]:
                if isinstance(sample, dict) and sample.get("phase") in ("extension", "final-window", "measured"):
                    if sample.get("resourcesComplete") is not True or sample.get("activity") not in ("quiescent", None):
                        _add(reasons, "terminal-incomplete")
        for sample in samples:
            if isinstance(sample, dict) and (sample.get("gpuCombinedIntoRss") is True or sample.get("vramBytes") is not None):
                _add(reasons, "gpu-combined")

    good = [endpoint for endpoint in endpoints if _endpoint_ok(endpoint, floor["settleSeconds"])]
    post = [row for row in facts["post"] if isinstance(row, dict)]
    same_state = bool(endpoints) and _same_states([*endpoints, *post])
    if status == "COMPLETED" and endpoints and not same_state:
        _add(reasons, "equivalent-state")
    analysis: dict[str, Any] = {}
    if status == "COMPLETED" and same_state and len(good) >= 3 and len(good) == len(endpoints):
        untouched = _heap_untouched(out)
        for key in ("rssBytes", "pssBytes"):
            stats = _series_growth(good, key)
            stats["trendSlopePerSwitch"] = _trend_slope(good, key, untouched)
            analysis[key] = stats
            if stats["slopePerSwitch"] is None or not _positive(stats["firstThirdMedian"]):
                _add(reasons, "missing-samples")
            elif stats["growth"] > stats["growthLimit"]:
                _add(reasons, "memory-growth")
            elif stats["trendSlopePerSwitch"] is None:
                _add(reasons, "missing-samples")
            elif stats["trendSlopePerSwitch"] > TREND_BYTES_PER_SWITCH:
                _add(reasons, "memory-trend")
        for key, limit, code in (("fdCount", FD_GROWTH_MAX, "fd-growth"), ("threadCount", THREAD_GROWTH_MAX, "thread-growth"), ("watchCount", WATCHER_GROWTH_MAX, "watcher-growth")):
            stats = _series_growth(good, key)
            analysis[key] = stats
            if stats["growth"] > limit:
                _add(reasons, code)
        for row in post:
            _judge_retained(row, analysis, floor["settleSeconds"], reasons)
    elif status == "COMPLETED" and (same_state or not endpoints):
        _add(reasons, "missing-samples")
    elif status == "COMPLETED" and post:
        for row in post:
            _judge_retained(row, {}, floor["settleSeconds"], reasons)
    out["analysis"] = analysis

    if profile == "long":
        windows = out.get("windows") if isinstance(out.get("windows"), dict) else {}
        observation = counts.get("observationSeconds")
        if not _finite(observation) or observation < LONG_OBSERVATION_SECONDS:
            _add(reasons, "undersized")
        if not facts["fromRaw"]:
            order = out.get("sampleOrder") if isinstance(out.get("sampleOrder"), list) else []
            final_rows = _window_rows(windows.get("final"))
            final_ids = [row.get("sampleId") for row in final_rows]
            if final_ids and order and order[-len(final_ids):] != final_ids:
                _add(reasons, "final-window-early")
        final_rows = _window_rows(windows.get("final"))
        base_rows = _window_rows(windows.get("baseline"))
        for name in ("baseline", "final"):
            window = windows.get(name) if isinstance(windows.get(name), dict) else {}
            rows = _window_rows(window)
            seconds = window.get("seconds")
            if not _finite(seconds) or seconds < LONG_WINDOW_SECONDS or len(rows) < WINDOW_MIN_SAMPLES:
                _add(reasons, "undersized")
            offsets = [row.get("offsetSec") for row in rows]
            if not offsets or not all(_finite(item) for item in offsets) or max(offsets) - min(offsets) < LONG_WINDOW_SECONDS - 1:
                _add(reasons, "undersized")
            _judge_window_rows(rows, reasons)
        window_states = [*base_rows, *final_rows]
        windows_match = bool(window_states) and _same_states(window_states) and (not endpoints or _state_key(endpoints[0]) == _state_key(window_states[0]))
        if window_states and not windows_match:
            _add(reasons, "equivalent-state")
        if base_rows and final_rows:
            if all(_positive(row.get("rssBytes")) for row in [*base_rows, *final_rows]):
                if _median([row["rssBytes"] for row in final_rows]) - _median([row["rssBytes"] for row in base_rows]) > _growth_limit(_median([row["rssBytes"] for row in base_rows])):
                    _add(reasons, "memory-growth")
            if all(_positive(row.get("pssBytes")) for row in [*base_rows, *final_rows]):
                if _median([row["pssBytes"] for row in final_rows]) - _median([row["pssBytes"] for row in base_rows]) > _growth_limit(_median([row["pssBytes"] for row in base_rows])):
                    _add(reasons, "memory-growth")
        _compare_window_counts(base_rows, final_rows, reasons, compare_watchers=windows_match)

    cleanup = out.get("cleanup") if isinstance(out.get("cleanup"), dict) else {}
    out["cleanup"] = cleanup
    survivors = cleanup.get("survivors") if isinstance(cleanup.get("survivors"), list) else []
    problems = cleanup.get("problems") if isinstance(cleanup.get("problems"), list) else []
    if survivors:
        _add(reasons, "survivor-child")
    if problems:
        _add(reasons, "cleanup-problems")
    harness_ok = cleanup.get("checked") is True and not survivors and not problems
    interactions = out.get("interactions") if isinstance(out.get("interactions"), list) else []
    out["interactions"] = interactions
    quit_rows = [row for row in interactions if isinstance(row, dict) and row.get("item") == "quit-cleanup"]
    quit_supported = bool(quit_rows) and all(row.get("ok") is True and _log_supports("quit-cleanup", row) for row in quit_rows)
    quit_claimed = any(row.get("ok") is True for row in quit_rows)
    if quit_claimed or cleanup.get("graceful") is True or cleanup.get("productQuit") is True:
        if cleanup.get("productQuit") is not True or cleanup.get("harnessForced") is True or cleanup.get("graceful") is False or not quit_supported:
            _add(reasons, "forced-quit-claimed")
    product_quit = (
        cleanup.get("productQuit") is True
        and cleanup.get("harnessForced") is not True
        and cleanup.get("graceful") is True
        and not survivors
        and not problems
        and quit_supported
        and not any(row.get("ok") is False for row in quit_rows)
    )

    def _item_ok(item: str) -> bool:
        matches = [row for row in interactions if isinstance(row, dict) and row.get("item") == item]
        if not matches:
            return False
        if any(row.get("ok") is not True for row in matches):
            return False
        return any(_log_supports(item, row) for row in matches)

    switches_ok = bool(measured) and all(_action_fields_ok(row) for row in measured)
    supported = {
        "repo-switch": switches_ok,
        "history": switches_ok and all(isinstance(row, dict) and "[APP:GRAPH_LOADED:" in str(row.get("graphLine")) for row in measured),
        "tree": _item_ok("tree"),
        "workspace-close-reopen": _item_ok("workspace-close-reopen"),
        "copy": _item_ok("copy"),
        "paste": _item_ok("paste"),
        "cancel": _item_ok("cancel"),
        "hide": False,
        "tray": False,
        "quit-cleanup": product_quit,
    }
    coverage: dict[str, Any] = {}
    gaps: list[str] = []
    for item in RELEASE_COVERAGE:
        coverage[item] = {
            "status": "exercised" if supported[item] else ("harness-forced" if item == "quit-cleanup" and cleanup.get("harnessForced") is True else "missing"),
            "requiredForShort": item in SHORT_COVERAGE,
            "requiredForRelease": True,
            "supportedByEvidence": supported[item],
        }
        if not supported[item]:
            gaps.append(item)
    if status == "COMPLETED" and any(not supported[item] for item in (RELEASE_COVERAGE if profile == "long" else SHORT_COVERAGE)):
        _add(reasons, "missing-coverage")
    out["coverage"] = coverage
    out["coverageGaps"] = gaps
    if out.get("evidenceClass") != "product-run" and profile == "long":
        _add(reasons, "synthetic-evidence")
    if out.get("evidenceClass") == "product-run" and report.get("synthetic") is True:
        _add(reasons, "synthetic-evidence")

    blocked = (
        "missing-samples", "nan-sample", "incomplete-resources", "raw-evidence", "raw-divergent",
        "identity-mismatch", "equivalent-state", "clipboard-state", "fake-tasks", "final-window-early",
    )
    subgates = {
        "memory": "NOT_ACCEPTED" if any(code in reasons for code in ("memory-growth", "memory-trend", "gpu-combined", *blocked)) else "ACCEPTED",
        "fd": "NOT_ACCEPTED" if any(code in reasons for code in ("fd-growth", *blocked)) else "ACCEPTED",
        "thread": "NOT_ACCEPTED" if any(code in reasons for code in ("thread-growth", *blocked)) else "ACCEPTED",
        "watchers": "NOT_ACCEPTED" if any(code in reasons for code in ("watcher-growth", *blocked)) else "ACCEPTED",
        "harnessCleanup": "ACCEPTED" if harness_ok else "NOT_ACCEPTED",
        "cleanup": "ACCEPTED" if harness_ok else "NOT_ACCEPTED",
    }
    if status != "COMPLETED":
        for key in ("memory", "fd", "thread", "watchers"):
            subgates[key] = "NOT_ACCEPTED"
    failing = list(reasons)
    if status == "COMPLETED" and profile == "short" and not failing and out.get("evidenceClass") != "product-run":
        verdict = "SUBGATE_ACCEPTED"
    elif (
        status == "COMPLETED"
        and profile == "short"
        and not failing
        and out.get("evidenceClass") == "product-run"
    ):
        verdict = "SUBGATE_ACCEPTED"
    elif status == "COMPLETED" and profile == "long" and not failing and out.get("evidenceClass") == "product-run":
        verdict = "LEAK_GATE_ACCEPTED"
    else:
        verdict = "NOT_ACCEPTED"
    out["verdict"] = verdict
    out["releaseOverall"] = "LEAK_GATE_ACCEPTED" if verdict == "LEAK_GATE_ACCEPTED" else "NOT_ACCEPTED"
    out["reasons"] = reasons
    out["subgates"] = subgates
    return out


def living_survivors(idents: list[dict[str, Any]]) -> list[dict[str, Any]]:
    alive = []
    for ident in idents:
        if isinstance(ident, dict) and ident.get("starttime") is not None and is_same_process(ident):
            alive.append({key: ident.get(key) for key in ("pid", "starttime", "exe", "comm")})
    return alive


def load_fixture(workspace: str) -> dict[str, Any]:
    manifest_path = os.path.join(workspace, "workload_manifest.json")
    with open(manifest_path, encoding="utf-8") as handle:
        manifest = json.load(handle)
    if not isinstance(manifest, dict):
        raise LeakError("workload manifest is not an object")
    names = [os.path.basename(path.rstrip("/")) for path in workspace_repos(workspace)]
    manifest_names: list[str] = []
    repos = manifest.get("repos") if isinstance(manifest.get("repos"), list) else []
    git_ok = True
    for row in repos:
        if isinstance(row, dict) and isinstance(row.get("name"), str):
            manifest_names.append(row["name"])
            repo_path = row.get("path") if isinstance(row.get("path"), str) else os.path.join(workspace, row["name"])
            if os.path.isdir(repo_path) and isinstance(row.get("commitCount"), int):
                try:
                    actual = int(git_readonly(repo_path, "rev-list", "--all", "--count").strip())
                except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError) as exc:
                    git_ok = False
                    row["gitError"] = str(exc)
                    continue
                if actual != row["commitCount"]:
                    git_ok = False
    return {
        "manifest": manifest,
        "sha256": sha256_file(manifest_path),
        "class": classify_workload(manifest),
        "filesystemRepos": names,
        "manifestRepos": manifest_names,
        "preset": manifest.get("preset"),
        "summary": manifest.get("summary"),
        "gitFactsMatch": git_ok and set(manifest_names) == set(names),
    }


def _receipt_view(receipt: dict[str, Any] | None) -> dict[str, Any] | None:
    if receipt is None:
        return None
    document = receipt.get("document") if isinstance(receipt.get("document"), dict) else {}
    return {
        "origin": receipt.get("origin"),
        "path": receipt.get("path"),
        "sha256MatchesBinary": receipt.get("sha256MatchesBinary"),
        "claimsSourceSha": document.get("sourceSha") or document.get("gitHead") or document.get("revision"),
        "sourceBuildAuthorized": False,
        "authorization": "rejected: a receipt cannot authorize that this binary was built from sourceSha",
    }


def assemble_report(args: argparse.Namespace) -> dict[str, Any]:
    profile = args.profile
    floor = FLOORS[profile]
    reasons: list[str] = []
    measured = args.measured_switches if args.measured_switches is not None else floor["measuredSwitches"]
    warmup = args.warmup_switches if args.warmup_switches is not None else floor["warmupSwitches"]
    settle = args.settle_seconds if args.settle_seconds is not None else floor["settleSeconds"]
    if not isinstance(measured, int) or isinstance(measured, bool) or measured < floor["measuredSwitches"]:
        _add(reasons, "undersized")
    if not isinstance(warmup, int) or isinstance(warmup, bool) or warmup < floor["warmupSwitches"]:
        _add(reasons, "undersized")
    if not _finite(settle):
        _add(reasons, "nan-sample")
    elif settle < floor["settleSeconds"]:
        _add(reasons, "undersized")
    if not args.expected_binary_sha256:
        _add(reasons, "wrong-binary")
    if not args.expected_fixture_sha256:
        _add(reasons, "wrong-fixture")
    if not args.expected_source_sha:
        _add(reasons, "wrong-revision")
    discovery = discover_standard_binary(REPO_ROOT)
    bin_path = os.path.realpath(args.bin) if args.bin else discovery.get("path")
    binary_sha = None
    if not bin_path or not os.path.isfile(bin_path):
        _add(reasons, "binary-not-found" if args.bin else "binary-discovery-failed")
    else:
        binary_sha = sha256_file(bin_path)
        if args.expected_binary_sha256 and args.expected_binary_sha256.lower() != binary_sha:
            _add(reasons, "wrong-binary")
    receipt_view = None
    if bin_path and os.path.isfile(bin_path) and (args.build_receipt or os.path.isfile(bin_path + ".receipt.json")):
        try:
            receipt_view = _receipt_view(load_build_receipt(bin_path, args.build_receipt))
        except (OSError, ValueError) as exc:
            _add(reasons, "wrong-binary")
            receipt_view = {"error": str(exc), "sourceBuildAuthorized": False}

    driver_source = source_sha_fact(REPO_ROOT)
    source = None
    if receipt_view and receipt_view.get("sha256MatchesBinary") is True and receipt_view.get("claimsSourceSha"):
        source = receipt_view["claimsSourceSha"]
    elif receipt_view and args.build_receipt:
        _add(reasons, "wrong-revision")
    else:
        source = driver_source

    if not args.expected_source_sha:
        _add(reasons, "wrong-revision")
    elif source is None or args.expected_source_sha != source:
        _add(reasons, "wrong-revision")
    fixture = None
    fixture_error = None
    try:
        fixture = load_fixture(args.workspace)
    except (OSError, ValueError, LeakError, json.JSONDecodeError) as exc:
        fixture_error = f"{type(exc).__name__}: {exc}"
        _add(reasons, "wrong-workload")
    if fixture is not None:
        if args.expected_fixture_sha256 and args.expected_fixture_sha256.lower() != fixture["sha256"]:
            _add(reasons, "wrong-fixture")
        if fixture["class"] == "other" or (profile == "long" and fixture["class"] != "standard-release"):
            _add(reasons, "wrong-workload")
        if not fixture["gitFactsMatch"] or set(fixture["manifestRepos"]) != set(fixture["filesystemRepos"]):
            _add(reasons, "wrong-workload")
        if len(fixture["filesystemRepos"]) != floor["repos"]:
            _add(reasons, "wrong-workload")
    if args.absolute_release_budget:
        _add(reasons, "absolute-budget-claimed")
    return {
        "schemaVersion": SCHEMA_VERSION,
        "check": CHECK_NAME,
        "generatedAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "profile": profile,
        "evidenceClass": "not-run",
        "driverStatus": "NOT_RUN",
        "runId": uuid.uuid4().hex,
        "sourceSha": source,
        "driverSourceSha": driver_source,
        "binarySha256": binary_sha,
        "fixtureSha256": None if fixture is None else fixture["sha256"],
        "pins": {
            "binarySha256": args.expected_binary_sha256.lower() if args.expected_binary_sha256 else None,
            "fixtureSha256": args.expected_fixture_sha256.lower() if args.expected_fixture_sha256 else None,
            "sourceSha": args.expected_source_sha,
        },
        "binary": {"path": bin_path, "sha256": binary_sha, "label": args.label, "buildProfile": args.build_profile, "selection": "explicit" if args.bin else "standard-discovery"},
        "binaryDiscovery": discovery,
        "buildReceipt": receipt_view,
        "sourceBuildAuthorized": False,
        "workload": {
            "path": os.path.abspath(args.workspace),
            "class": None if fixture is None else fixture["class"],
            "preset": None if fixture is None else fixture["preset"],
            "repoCount": 0 if fixture is None else len(fixture["filesystemRepos"]),
            "repos": [] if fixture is None else list(fixture["filesystemRepos"]),
            "summary": None if fixture is None else fixture["summary"],
            "gitFactsMatch": False if fixture is None else fixture["gitFactsMatch"],
            "error": fixture_error,
            "releaseWorkload": bool(fixture and fixture["class"] == "standard-release" and fixture["gitFactsMatch"]),
        },
        "counts": {
            "warmupSwitchesRequested": warmup,
            "warmupSwitchesActual": None,
            "measuredSwitchesRequested": measured,
            "measuredSwitchesActual": None,
            "reposExpected": floor["repos"],
            "reposChecked": 0,
            "checkpoints": 0,
            "quiescentCheckpoints": 0,
            "settleSeconds": settle,
            "observationSeconds": None,
        },
        "samples": [],
        "sampleOrder": [],
        "evidence": {"measuredSwitches": [], "warmupSwitches": []},
        "interactions": [],
        "heapReserve": [],
        "rawSamples": None,
        "coverage": {},
        "thresholds": dict(THRESHOLDS),
        "floors": dict(floor),
        "cleanup": {"checked": False, "problems": [], "survivors": [], "harnessForced": False, "productQuit": False, "graceful": False},
        "reasons": reasons,
        "absoluteReleaseBudgetClaimed": False,
        "productAcceptance": False,
        "releaseComplete": False,
        "d4": "NOT_EVALUATED",
        "integration": {"check": CHECK_NAME, "wired": False},
    }


def _append_jsonl(path: str, row: dict[str, Any]) -> None:
    with open(path, "a", encoding="utf-8") as handle:
        json.dump(row, handle, allow_nan=False)
        handle.write("\n")


def _exclude_controllers(session: NativeSession) -> set[int]:
    app_pids = set(session.app_tree_pids())
    exclude = {session.xvfb.pid, session.proc.pid}
    for pid in descendants(session.proc.pid):
        if pid not in app_pids:
            exclude.add(pid)
    for pid in descendants(session.xvfb.pid):
        exclude.add(pid)
    if session.app:
        exclude.discard(session.app["pid"])
    return exclude


def _sample(session: NativeSession, exclude: set[int]) -> dict[str, Any]:
    if session.app is None:
        raise LeakError("app identity is missing")
    return sample_app_resources(
        session.app["pid"],
        expected_exe=session.bin_path,
        expected_starttime=session.app["starttime"],
        exclude_pids=exclude,
    )


def _flatten(snap: dict[str, Any], meta: dict[str, Any]) -> dict[str, Any]:
    totals = snap.get("totals") or {}
    git_children = totals.get("gitChildren")
    activity = snap.get("activity")
    if activity not in ("busy", "quiescent", "unreadable"):
        activity = "unreadable" if git_children is None else ("busy" if git_children else "quiescent")
    return {
        "sampleId": meta["sampleId"],
        "batchIndex": meta["batchIndex"],
        "warmup": meta["warmup"],
        "phase": meta["phase"],
        "activity": activity,
        "resourcesComplete": snap.get("resourcesComplete") is True,
        "memoryComplete": snap.get("memoryComplete") is True,
        "measuredSwitchesCompleted": meta["measured"],
        "settleSeconds": meta["settle"],
        "equivalentView": meta["view"],
        "viewEvidence": meta["view_evidence"],
        "rssBytes": totals.get("rssBytes"),
        "pssBytes": totals.get("pssBytes"),
        "fdCount": totals.get("fdCount"),
        "threadCount": totals.get("threadCount"),
        "watchCount": totals.get("watchCount"),
        "gitChildren": git_children,
        "processCount": totals.get("processCount"),
        "unreadablePids": list(snap.get("unreadablePids") or []),
        "root": snap.get("root"),
        "gpuCombinedIntoRss": False,
        "vramBytes": None,
        "rssProvenance": snap.get("rssProvenance"),
        "pssProvenance": snap.get("pssProvenance"),
        "ownedTasks": None,
    }


def _canonical_view(repo: str) -> dict[str, Any]:
    return {"repo": repo, "tool": "GitChanges", "workspaceOpen": True, "closeObserved": False}


def _publish_gate_and_wait(session: NativeSession) -> dict[str, Any]:
    def on_app(_ident: dict[str, Any]) -> None:
        with open(session.sampler_ready_file, "w", encoding="utf-8") as handle:
            handle.write("ready\n")
            handle.flush()
            os.fsync(handle.fileno())

    ident = session.wait_app(on_app, timeout=60.0)
    deadline = time.monotonic() + 30.0
    while time.monotonic() < deadline:
        current = identity(ident["pid"])
        if current["starttime"] != ident["starttime"]:
            raise LeakError("app pid was reused before exec")
        if current["exe"] == session.bin_path and current["starttime"] == ident["starttime"]:
            session.app = current
            return current
        time.sleep(0.01)
    raise LeakError(f"process {ident['pid']} did not exec {session.bin_path}")


def _action_from_click(session: NativeSession, repo: str, phase: str, sent: float, t_loaded: float, t_graph: float, before: int) -> dict[str, Any]:
    fresh = session.texts(before)
    selecting = [line for line in fresh if "[APP:REPO_SELECTING:" in line]
    if not selecting:
        raise LeakError("fresh slice has no REPO_SELECTING")
    _idx, name = parse_repo_select(selecting[-1])
    loaded = [line for line in fresh if f"[APP:REPO_LOADED: {name} files=" in line]
    graphs = [line for line in fresh if "[APP:GRAPH_LOADED:" in line]
    if not loaded or not graphs:
        raise LeakError(f"fresh slice for {name} lacks load or graph")
    files = int(re.search(r"files=(\d+)", loaded[-1])[1])
    try:
        state = check_repo_state(fresh, repo_oracle(repo))
        oracle = {"ok": True, "sourceRows": state["sourceRows"], "reportedFiles": files, "distinctPaths": state["distinctPaths"]}
    except NativeBenchError as exc:
        oracle = {"ok": False, "error": str(exc), "reportedFiles": files}
    return {
        "actionId": uuid.uuid4().hex,
        "phase": phase,
        "repo": name,
        "input": "click",
        "repoLoaded": True,
        "graphLoaded": True,
        "selectLine": selecting[-1],
        "loadedLine": loaded[-1],
        "graphLine": graphs[-1],
        "clickToRepoLoadedMs": (t_loaded - sent) * 1000,
        "clickToGraphLoadedMs": (t_graph - sent) * 1000,
        "oracle": oracle,
        "root": {key: session.app.get(key) for key in ("pid", "starttime", "exe", "comm")} if session.app else None,
    }


def _equivalent_view(session: NativeSession, win: dict[str, Any], repo: str) -> dict[str, Any]:
    sent, t_loaded, t_graph, before = click_repo(session, win, repo)
    show_changes(session, win)
    fresh = session.texts(before)
    name = os.path.basename(repo.rstrip("/"))
    selecting = [line for line in fresh if "[APP:REPO_SELECTING:" in line]
    loaded = [line for line in fresh if f"[APP:REPO_LOADED: {name} files=" in line]
    graphs = [line for line in fresh if "[APP:GRAPH_LOADED:" in line]
    tabs = [line for line in fresh if "TAB_SWITCHED: GitChanges" in line and "visible=true" in line]
    if not selecting or not loaded or not graphs or not tabs:
        raise LeakError("equivalent view did not produce a fresh select, load, graph, and GitChanges tab")
    return {
        "repo": name,
        "tool": "GitChanges",
        "selectLine": selecting[-1],
        "loadedLine": loaded[-1],
        "graphLine": graphs[-1],
        "tabLine": tabs[-1],
        "repoLoaded": True,
        "tab": "GitChanges",
        "workspaceOpen": True,
        "closeObserved": False,
        "clickToRepoLoadedMs": (t_loaded - sent) * 1000,
    }


def _settle(session: NativeSession, exclude: set[int], seconds: float, meta: dict[str, Any]) -> list[dict[str, Any]]:
    start = time.monotonic()
    rows: list[dict[str, Any]] = []

    def take() -> None:
        snap = _sample(session, exclude)
        rows.append(_flatten(snap, {**meta, "sampleId": uuid.uuid4().hex, "settle": round(time.monotonic() - start, 3)}))

    for offset in (0.0, seconds / 2.0, seconds):
        delay = start + offset - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        take()
    deadline = start + seconds + 10.0
    while rows and not _resource_settled(rows[-1]) and time.monotonic() < deadline:
        time.sleep(0.5)
        take()
    if time.monotonic() - start + 0.05 < seconds:
        raise LeakError("settle window ended before the minimum")
    return rows


def heap_mapping(smaps: bytes) -> dict[str, int] | None:
    """Address-space size and resident bytes of the main-arena `[heap]` mapping, if any."""
    size = rss = None
    for line in smaps.decode(errors="replace").splitlines():
        head = line.split()
        if len(head) >= 6 and "-" in head[0] and head[-1] == "[heap]":
            start, end = (int(part, 16) for part in head[0].split("-"))
            size = end - start
        elif size is not None and head[:1] == ["Rss:"]:
            rss = int(head[1]) * 1024
            break
    if size is None or rss is None:
        return None
    return {"heapVmaBytes": size, "heapRssBytes": rss}


def _capture_proc_snapshot(app: dict[str, Any], sample: dict[str, Any], out_dir: str, proc_root: str = "/proc") -> dict[str, int] | None:
    """Read diagnostic sidecars after an endpoint is emitted, outside its settle window."""
    keys = ("pid", "starttime", "exe")
    expected = {key: app.get(key) for key in keys}
    if any(value is None for value in expected.values()):
        raise LeakError("proc snapshot requires a complete owned app identity")
    pid = expected["pid"]
    started = time.monotonic()
    try:
        before = identity(pid, proc_root)
        if any(before.get(key) != expected[key] for key in keys):
            raise LeakError("proc snapshot app identity changed before read")
        contents = {}
        for name in ("smaps", "status"):
            with open(os.path.join(proc_root, str(pid), name), "rb") as handle:
                contents[name] = handle.read()
            if not contents[name]:
                raise LeakError(f"proc snapshot {name} was empty")
        uname = os.uname()
        runtime = {
            "cpuCount": os.cpu_count(),
            "appCpuAffinity": sorted(os.sched_getaffinity(pid)),
            "uname": {key: getattr(uname, key) for key in ("sysname", "release", "version", "machine")},
        }
        after = identity(pid, proc_root)
        if any(after.get(key) != expected[key] for key in keys):
            raise LeakError("proc snapshot app identity changed during read")
        finished = time.monotonic()
        directory = os.path.join(out_dir, "proc-snapshots")
        os.makedirs(directory, exist_ok=True)
        files = {}
        for name, data in contents.items():
            relative = os.path.join("proc-snapshots", f"{sample['seq']:05d}-{sample['kind']}.{name}")
            with open(os.path.join(out_dir, relative), "xb") as handle:
                handle.write(data)
            files[name] = relative
        _append_jsonl(os.path.join(directory, "index.jsonl"), {
            "sampleId": sample["sampleId"], "seq": sample["seq"], "kind": sample["kind"],
            "measuredSwitchesCompleted": sample["measuredSwitchesCompleted"],
            "sampleTMono": sample["tMono"], "captureStartMono": started, "captureEndMono": finished,
            "sampleEmitLagSeconds": started - sample["tMono"],
            "identity": expected, "runtime": runtime, "files": files,
        })
    except OSError as exc:
        raise LeakError(f"proc snapshot failed for owned app {pid}: {exc}") from exc
    return heap_mapping(contents["smaps"])


def _resource_settled(row: dict[str, Any]) -> bool:
    return row.get("resourcesComplete") is True and row.get("gitChildren") == 0


def _try_tree(session: NativeSession, win: dict[str, Any], interactions: list[dict[str, Any]], note) -> None:
    try:
        open_project_list(session, win)
        used = {row.get("control") for row in interactions if isinstance(row, dict) and row.get("item") == "tree"}
        # The tab switch is logged before the frame that paints its rows.
        deadline = time.monotonic() + 10
        while True:
            tree_ids = [control for control in sorted(parse_bounds(session.texts())) if control.startswith("tree-row:") and control not in used]
            if tree_ids or time.monotonic() >= deadline:
                break
            time.sleep(0.1)
        if not tree_ids:
            note(item="tree", ok=False, input="click", log="", reason="no tree-row probe")
            return
        bounds = scroll_into_view(session, win, tree_ids[0])
        before = len(session.lines)
        session.click(win, bounds)
        _index, _when, line = session.wait_line(
            lambda text: "TREE_FILE_SELECTED" in text or "TREE_EXPANDED" in text or "TREE_TOGGLED" in text,
            start=before, timeout=8,
        )
        if "TREE_FILE_SELECTED" in line:
            # A plain click on a file also selects it into the basket; Ctrl-click
            # toggles it back out so the copy item starts from an empty basket.
            again = len(session.lines)
            session.x("xdotool", "keydown", "ctrl")
            try:
                session.click(win, bounds)
            finally:
                session.x("xdotool", "keyup", "ctrl")
            session.wait_line(lambda text: "[APP:BASKET: n=0" in text, start=again, timeout=8)
        note(item="tree", ok=True, input="click", control=tree_ids[0], log=line, root=_root_dict(session))
    except (NativeBenchError, LeakError) as exc:
        note(item="tree", ok=False, input="click", log="", reason=str(exc))


def _root_dict(session: NativeSession) -> dict[str, Any] | None:
    if session.app is None:
        return None
    return {key: session.app.get(key) for key in ("pid", "starttime", "exe", "comm")}


def _try_copy_paste(session: NativeSession, win: dict[str, Any], repo: str, note) -> str | None:
    checkbox: str | None = None
    try:
        oracle = repo_oracle(repo)
        result = copy_explicit_selection(session, win, oracle)
        controls = result.get("controls") if isinstance(result.get("controls"), dict) else {}
        if isinstance(controls.get("checkbox"), str):
            checkbox = controls["checkbox"]
        copy_line = next((line for line in reversed(session.texts()) if "[APP:COPY_DONE:" in line), "")
        note(
            item="copy", ok=True, input="click", log=copy_line,
            oracle={"verified": result.get("verified") is True}, root=_root_dict(session), control=checkbox,
        )
    except (NativeBenchError, LeakError, OSError) as exc:
        note(item="copy", ok=False, input="click", log="", reason=str(exc))
    try:
        before = len(session.lines)
        session.key(win["wid"], "ctrl+v")
        _index, _when, line = session.wait_line(
            lambda text: "PASTE_PREVIEW" in text or "PASTE_DONE" in text, start=before, timeout=8,
        )
        note(item="paste", ok=True, input="key", log=line, applied=False, root=_root_dict(session))
    except (NativeBenchError, LeakError) as exc:
        note(item="paste", ok=False, input="key", log="", reason=str(exc), applied=False)
    try:
        # Escape cancels only with the paste panel focused, so wait for the button instead.
        before = len(session.lines)
        _click_control_when_ready(session, win, "btn-cancel", 6.0)
        _index, _when, line = session.wait_line(lambda text: "PASTE_CANCELLED" in text, start=before, timeout=8)
        note(item="cancel", ok=True, input="click", control="btn-cancel", log=line, root=_root_dict(session))
    except (NativeBenchError, LeakError) as exc:
        note(item="cancel", ok=False, input="click", log="", reason=str(exc))
    return checkbox


def _restore_basket(session: NativeSession, win: dict[str, Any], checkbox_id: str | None, note) -> None:
    if not checkbox_id:
        note(item="basket-restore", ok=False, input="click", log="", reason="copy did not return a checkbox; basket was not forged")
        return
    try:
        bounds = scroll_into_view(session, win, checkbox_id)
        before = len(session.lines)
        session.click(win, bounds)
        _index, _when, line = session.wait_line(lambda text: "[APP:BASKET: n=0" in text, start=before, timeout=10)
        note(item="basket-restore", ok=True, input="click", control=checkbox_id, log=line, root=_root_dict(session))
    except (NativeBenchError, LeakError) as exc:
        note(item="basket-restore", ok=False, input="click", control=checkbox_id, log="", reason=str(exc))


def _click_control_when_ready(
    session: NativeSession, win: dict[str, Any], control_id: str, timeout: float = 6.0
) -> None:
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        bounds = parse_bounds(session.texts()).get(control_id)
        if bounds is not None:
            session.click(win, bounds)
            return
        time.sleep(0.1)
    raise NativeBenchError(f"control {control_id} not found in bounds within {timeout}s")


def _wait_for_control(session: NativeSession, control_id: str, timeout: float = 6.0) -> None:
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        if control_id in parse_bounds(session.texts()):
            return
        time.sleep(0.1)
    raise NativeBenchError(f"control {control_id} did not appear within {timeout}s")


def is_drained(line: str, intent: str = "close-workspace") -> bool:
    return (
        f"phase=drained intent={intent}" in line
        and "jobs=0" in line
        and "inflight=0" in line
        and "queued=0" in line
        and "leaked=0" in line
    )


def _track_app_descendants(session: NativeSession, tracked: dict[tuple[int, int], dict[str, Any]]) -> None:
    if session.app is None:
        return
    app_pid = session.app.get("pid") if isinstance(session.app, dict) else None
    if app_pid is None:
        return
    exclude = _exclude_controllers(session)
    if session.proc is not None and hasattr(session.proc, "pid"):
        try:
            for p in descendants(session.proc.pid):
                if p != app_pid and p not in exclude:
                    ident = identity(p)
                    if ident.get("starttime") is not None:
                        tracked[(ident["pid"], ident["starttime"])] = ident
        except Exception:
            pass
    try:
        for p in descendants(app_pid):
            ident = identity(p)
            if ident.get("starttime") is not None:
                tracked[(ident["pid"], ident["starttime"])] = ident
    except Exception:
        pass


def _try_workspace_close_reopen(
    session: NativeSession,
    win: dict[str, Any],
    workspace_path: str,
    expected_repo_count: int,
    tracked_descendants: dict[tuple[int, int], dict[str, Any]],
    note: Any,
) -> None:
    try:
        before_clip = _read_clip(session)

        # Record cursor before triggering close click
        cursor_close = len(session.lines)

        bounds = parse_bounds(session.texts())
        if "btn-close-workspace" not in bounds and "btn-workspace-menu" in bounds:
            session.click(win, bounds["btn-workspace-menu"])
        _click_control_when_ready(session, win, "btn-close-workspace", timeout=6.0)

        _idx, _t, closed_line = session.wait_line(
            lambda line: "[APP:WORKSPACE: state=closed" in line, start=cursor_close, timeout=12.0
        )
        fresh_close_texts = session.texts(cursor_close)
        drained_log = any(is_drained(l, "close-workspace") for l in fresh_close_texts)
        _wait_for_control(session, "workspace-closed", timeout=6.0)

        same_proc_closed = is_same_process(session.app)
        clip_closed = _read_clip(session)
        clip_preserved = (
            isinstance(before_clip.get("sha256"), str)
            and before_clip.get("sha256") == clip_closed.get("sha256")
        )

        _track_app_descendants(session, tracked_descendants)
        living_app_descendants = [ident for ident in tracked_descendants.values() if is_same_process(ident)]
        no_surviving_descendants_closed = (len(living_app_descendants) == 0)

        # Record cursor before triggering reopen click
        cursor_open = len(session.lines)

        bounds = parse_bounds(session.texts())
        if "btn-open-workspace" not in bounds and "btn-workspace-menu" in bounds:
            session.click(win, bounds["btn-workspace-menu"])
        _click_control_when_ready(session, win, "btn-open-workspace", timeout=6.0)
        _click_control_when_ready(session, win, "workspace-path-input", timeout=6.0)
        session.focus(win["wid"])
        session.x("xdotool", "type", "--delay", "15", "--window", win["wid"], workspace_path)
        _click_control_when_ready(session, win, "btn-workspace-open-confirm", timeout=6.0)

        _idx_open, _t, opened_line = session.wait_line(
            lambda line: "[APP:WORKSPACE: state=open" in line and workspace_path in line,
            start=cursor_open, timeout=12.0,
        )
        _idx_ready, _t, ready_line = session.wait_line(
            lambda line: "[APP:READY_REPOS:" in line,
            start=cursor_open, timeout=30.0,
        )
        m = re.search(r"READY_REPOS:\s*(\d+)", ready_line)
        reopened_count = int(m.group(1)) if m else -1
        repos_ready = (reopened_count == expected_repo_count)

        same_proc_opened = is_same_process(session.app)
        clip_after = _read_clip(session)
        clip_still_preserved = (
            isinstance(before_clip.get("sha256"), str)
            and before_clip.get("sha256") == clip_after.get("sha256")
        )

        drained = bool(drained_log and no_surviving_descendants_closed)
        ok = bool(
            same_proc_closed
            and same_proc_opened
            and clip_preserved
            and clip_still_preserved
            and drained
            and repos_ready
        )
        log_combined = f"{closed_line} | {opened_line} | {ready_line}"
        oracle = {
            "sameProcess": same_proc_closed and same_proc_opened,
            "clipboardPreserved": clip_preserved and clip_still_preserved,
            "drained": drained,
            "reposReady": repos_ready,
        }
        note(
            item="workspace-close-reopen",
            ok=ok,
            input="click",
            control="btn-close-workspace",
            log=log_combined,
            oracle=oracle,
            root=_root_dict(session),
        )
    except (NativeBenchError, LeakError, OSError, subprocess.CalledProcessError) as exc:
        note(
            item="workspace-close-reopen",
            ok=False,
            input="click",
            control="btn-close-workspace",
            log="",
            reason=str(exc),
            root=_root_dict(session),
        )


def _read_clip(session: NativeSession) -> dict[str, Any]:
    try:
        payload = session.read_clipboard(timeout=5)
    except (NativeBenchError, subprocess.TimeoutExpired, OSError) as exc:
        return {"bytes": None, "sha256": None, "cleared": False, "error": str(exc)}
    return {"bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest(), "cleared": False}


def _hold_window(session: NativeSession, exclude: set[int], seconds: float, emit, kind: str, view: dict[str, Any], measured: int) -> dict[str, Any]:
    start = time.monotonic()
    rows: list[dict[str, Any]] = []
    while True:
        elapsed = time.monotonic() - start
        if elapsed >= seconds and rows:
            break
        snap = _sample(session, exclude)
        flat = _flatten(snap, {
            "sampleId": uuid.uuid4().hex,
            "batchIndex": -1,
            "warmup": False,
            "phase": kind,
            "measured": measured,
            "settle": round(time.monotonic() - start, 3),
            "view": _canonical_view(view["repo"]) if isinstance(view.get("repo"), str) else view,
            "view_evidence": view,
        })
        flat["offsetSec"] = round(time.monotonic() - start, 3)
        rows.append(emit({"kind": kind, **flat}))
        remaining = seconds - (time.monotonic() - start)
        if remaining > 0:
            time.sleep(min(1.0, remaining))
    return {"seconds": round(time.monotonic() - start, 3), "samples": rows}


def drive_product(report: dict[str, Any], out_dir: str) -> None:
    repos = workspace_repos(report["workload"]["path"])
    names = [os.path.basename(path.rstrip("/")) for path in repos]
    if names != report["workload"]["repos"]:
        raise LeakError("workspace repo list changed after preflight")
    run_dir = os.path.join(out_dir, "session")
    os.makedirs(run_dir, exist_ok=True)
    jsonl_path = os.path.join(out_dir, "raw_samples.jsonl")
    report["rawSamples"] = {"path": jsonl_path}
    run_id = report["runId"]
    settle = float(report["counts"]["settleSeconds"])
    session: NativeSession | None = None
    owned: list[dict[str, Any]] = []
    tracked_descendants: dict[tuple[int, int], dict[str, Any]] = {}
    pre_stop_survivors: list[dict[str, Any]] = []
    quit_ok = False
    state = {"seq": 0}

    def emit(row: dict[str, Any]) -> dict[str, Any]:
        payload = {"runId": run_id, "seq": state["seq"], "tMono": time.monotonic(), **row}
        state["seq"] += 1
        _append_jsonl(jsonl_path, payload)
        return payload

    def note(**fields: Any) -> None:
        report["interactions"].append(emit({"kind": "interaction", **fields}))

    def emit_sample(kind: str, row: dict[str, Any]) -> dict[str, Any]:
        if session is not None:
            _track_app_descendants(session, tracked_descendants)
        payload = emit({"kind": kind, **row})
        report["samples"].append(payload)
        report["sampleOrder"].append(payload["sampleId"])
        if kind in ("endpoint", "terminal") and session is not None:
            heap = _capture_proc_snapshot(session.app, payload, out_dir)
            if heap is not None:
                report["heapReserve"].append({"sampleId": payload["sampleId"], **heap})
        return payload

    try:
        session = NativeSession(report["binary"]["path"], report["workload"]["path"], "normal", run_dir, True)
        app = _publish_gate_and_wait(session)
        report["app"] = {key: app.get(key) for key in ("pid", "starttime", "exe", "comm")}
        session.wait_line(lambda line: "[APP:WINDOW_READY]" in line, timeout=120)
        _index, _when, ready = session.wait_line(lambda line: "[APP:READY_REPOS:" in line, timeout=180)
        match = re.search(r"READY_REPOS:\s*(\d+)", ready)
        if match is None or int(match.group(1)) != len(repos):
            raise LeakError(f"app repo-ready line {ready!r} does not match {len(repos)} filesystem repos")
        win = session.window()
        report["window"] = {key: win.get(key) for key in ("wid", "x", "y", "width", "height", "mapState")}
        by_name = {os.path.basename(path.rstrip("/")): path for path in repos}
        canonical = by_name[names[0]]
        # Fixed payload on the isolated display. Do not clear it to satisfy a memory budget.
        session.set_clipboard(CANONICAL_CLIPBOARD)
        baseline_clip = _read_clip(session)
        if baseline_clip.get("sha256") != hashlib.sha256(CANONICAL_CLIPBOARD).hexdigest():
            raise LeakError("canonical clipboard payload did not stick on the isolated display")
        report["clipboard"] = {"baseline": emit({"kind": "clipboard", "phase": "baseline", **baseline_clip}), "terminal": None}
        measured_done = 0
        observation_start = None
        report["windows"] = {"baseline": None, "final": None}
        canonical_view: dict[str, Any] | None = None
        for phase, total in (("warmup", report["counts"]["warmupSwitchesRequested"]), ("measured", report["counts"]["measuredSwitchesRequested"])):
            if phase == "measured":
                observation_start = time.monotonic()
                if report["profile"] == "long":
                    canonical_view = _equivalent_view(session, win, canonical)
                    report["windows"]["baseline"] = _hold_window(
                        session, _exclude_controllers(session), LONG_WINDOW_SECONDS, emit, "baseline", canonical_view, measured_done,
                    )
            pieces = phase_batches(names, total)
            for batch_index, piece in enumerate(pieces):
                print(f"[{phase}] switches {measured_done + 1 if phase == 'measured' else batch_index * BATCH_SWITCHES + 1} {piece[0]}..{piece[-1]}", flush=True)
                bucket = report["evidence"]["warmupSwitches" if phase == "warmup" else "measuredSwitches"]
                for name in piece:
                    sent, t_loaded, t_graph, before = click_repo(session, win, by_name[name])
                    action = _action_from_click(session, by_name[name], phase, sent, t_loaded, t_graph, before)
                    bucket.append(emit({"kind": "action", **action}))
                    if phase == "measured":
                        measured_done += 1
                canonical_view = _equivalent_view(session, win, canonical)
                rows = _settle(session, _exclude_controllers(session), settle, {
                    "batchIndex": batch_index,
                    "warmup": phase == "warmup",
                    "phase": phase,
                    "measured": measured_done,
                    "view": _canonical_view(canonical_view["repo"]),
                    "view_evidence": canonical_view,
                })
                for row in rows:
                    kind = "endpoint" if row is rows[-1] and phase == "measured" else "settle-point"
                    emit_sample(kind, row)
        for _ in range(3):
            _track_app_descendants(session, tracked_descendants)
            _try_tree(session, win, report["interactions"], note)
        for _ in range(3):
            _track_app_descendants(session, tracked_descendants)
            checkbox = _try_copy_paste(session, win, canonical, note)
            _restore_basket(session, win, checkbox, note)
        _track_app_descendants(session, tracked_descendants)
        _try_workspace_close_reopen(session, win, report["workload"]["path"], len(repos), tracked_descendants, note)
        for item, reason in (
            ("hide", "no window-hide contract; btn-log-hide is not hide"),
            ("tray", "no tray contract in this binary"),
        ):
            note(item=item, ok=False, input="click", log="", reason=reason)
        try:
            canonical_view = _equivalent_view(session, win, canonical)
        except (NativeBenchError, LeakError) as exc:
            note(item="equivalent-view", ok=False, input="click", log="", reason=str(exc))
            canonical_view = {
                "repo": names[0], "tool": "GitChanges", "workspaceOpen": True, "closeObserved": False,
                "selectLine": "", "loadedLine": "", "graphLine": "", "tabLine": "",
            }
        terminal_clip = _read_clip(session)
        copy_ok = any(row.get("item") == "copy" and row.get("ok") is True for row in report["interactions"])
        attribution = None
        if (
            copy_ok
            and isinstance(terminal_clip.get("sha256"), str)
            and terminal_clip.get("sha256") != baseline_clip.get("sha256")
            and isinstance(terminal_clip.get("bytes"), int)
            and terminal_clip["bytes"] > 0
        ):
            attribution = {
                "legitimate": True,
                "source": "explicit-copy",
                "sha256": terminal_clip["sha256"],
                "bytes": terminal_clip["bytes"],
            }
        if isinstance(terminal_clip.get("bytes"), int) and isinstance(terminal_clip.get("sha256"), str):
            report["clipboard"]["terminal"] = emit({
                "kind": "clipboard", "phase": "terminal", "attribution": attribution, **{key: terminal_clip[key] for key in ("bytes", "sha256", "cleared")},
            })
        else:
            note(item="clipboard", ok=False, input="key", log="", reason=str(terminal_clip.get("error") or "clipboard unreadable"))
        if canonical_view is not None:
            rows = _settle(session, _exclude_controllers(session), settle, {
                "batchIndex": -1,
                "warmup": False,
                "phase": "terminal",
                "measured": measured_done,
                "view": _canonical_view(str(canonical_view.get("repo") or names[0])),
                "view_evidence": canonical_view,
            })
            for row in rows:
                emit_sample("terminal" if row is rows[-1] else "settle-point", row)
        if report["profile"] == "long" and observation_start is not None and canonical_view is not None:
            # Final 30s window is after every measured operation. Lifecycle coverage is still missing,
            # so main() refuses to start this profile; the order stays correct if that refusal is lifted.
            while time.monotonic() - observation_start < LONG_OBSERVATION_SECONDS - LONG_WINDOW_SECONDS:
                time.sleep(1.0)
                snap = _sample(session, _exclude_controllers(session))
                flat = _flatten(snap, {
                    "sampleId": uuid.uuid4().hex, "batchIndex": -1, "warmup": False, "phase": "extension",
                    "measured": measured_done, "settle": 0,
                    "view": _canonical_view(str(canonical_view.get("repo") or names[0])),
                    "view_evidence": canonical_view,
                })
                emit_sample("extension", flat)
            report["windows"]["final"] = _hold_window(
                session, _exclude_controllers(session), LONG_WINDOW_SECONDS, emit, "final", canonical_view, measured_done,
            )
        if observation_start is not None:
            report["counts"]["observationSeconds"] = round(time.monotonic() - observation_start, 3)
        report["evidenceClass"] = "product-run"
        report["driverStatus"] = "COMPLETED"

        # Final alive resource sample has been recorded.
        # Track descendants before triggering Ctrl+Q
        _track_app_descendants(session, tracked_descendants)

        # Record cursor before triggering Ctrl+Q
        cursor_quit = len(session.lines)
        session.key(win["wid"], "ctrl+q")

        quit_log_lines: list[str] = []
        has_quit_deferred = False
        try:
            _idx, _t, quit_line = session.wait_line(
                lambda l: "[APP:QUIT: deferred]" in l, start=cursor_quit, timeout=6.0
            )
            quit_log_lines.append(quit_line)
            has_quit_deferred = True
        except (NativeBenchError, LeakError):
            quit_log_lines.append("no quit deferred log")

        has_quit_drained = False
        try:
            _idx, _t, drained_line = session.wait_line(
                lambda l: is_drained(l, "quit"), start=cursor_quit, timeout=6.0
            )
            quit_log_lines.append(drained_line)
            has_quit_drained = True
        except (NativeBenchError, LeakError):
            if any(is_drained(l, "quit") for l in session.texts(cursor_quit)):
                has_quit_drained = True
            else:
                quit_log_lines.append("no quit drained log")

        quit_log = " | ".join(quit_log_lines)

        exit_code = None
        try:
            exit_code = session.proc.wait(timeout=10.0)
        except subprocess.TimeoutExpired:
            exit_code = None

        app_dead = False
        if session.app is not None:
            app_dead = not is_same_process(session.app)
        elif session.proc is not None:
            app_dead = (session.proc.poll() is not None)

        # Record app-descendant identities alive before session.stop() force-kills anything
        _track_app_descendants(session, tracked_descendants)
        for ident in tracked_descendants.values():
            if is_same_process(ident):
                if not any(s.get("pid") == ident.get("pid") for s in pre_stop_survivors):
                    pre_stop_survivors.append(ident)

        exclude = _exclude_controllers(session)
        if session.proc is not None and hasattr(session.proc, "pid"):
            try:
                for p in descendants(session.proc.pid):
                    if p not in exclude and (session.app is None or p != session.app.get("pid")):
                        ident = identity(p)
                        if ident.get("starttime") is not None and is_same_process(ident):
                            if not any(s.get("pid") == ident.get("pid") for s in pre_stop_survivors):
                                pre_stop_survivors.append(ident)
            except Exception:
                pass

        quit_evidence = bool(has_quit_deferred and has_quit_drained)
        quit_ok = bool(
            exit_code == 0
            and app_dead
            and not pre_stop_survivors
            and quit_evidence
        )

        oracle = {
            "exitCode": exit_code,
            "appDead": app_dead,
            "survivorsPreStop": len(pre_stop_survivors),
            "drained": has_quit_drained,
            "deferred": has_quit_deferred,
        }

        if quit_ok:
            note(
                item="quit-cleanup",
                ok=True,
                input="key",
                control="ctrl+q",
                log=quit_log,
                oracle=oracle,
                root=_root_dict(session),
            )
        else:
            note(
                item="quit-cleanup",
                ok=False,
                input="key",
                control="ctrl+q",
                log=quit_log,
                oracle=oracle,
                reason=(
                    f"exit_code={exit_code}, app_dead={app_dead}, "
                    f"pre_stop_survivors={len(pre_stop_survivors)}, "
                    f"quit_evidence={quit_evidence}"
                ),
                root=_root_dict(session),
            )
    except Exception as exc:  # noqa: BLE001
        note(
            item="quit-cleanup",
            ok=False,
            input="key",
            control="ctrl+q",
            log=quit_log if "quit_log" in locals() else "",
            reason=str(exc),
            root=_root_dict(session) if session is not None else None,
        )
        raise
    finally:
        binary = report.get("binary") if isinstance(report.get("binary"), dict) else None
        if binary is not None:
            path = binary.get("path")
            binary["sha256After"] = sha256_file(path) if isinstance(path, str) and os.path.isfile(path) else None
        problems: list[str] = []
        post_stop_survivors: list[dict[str, Any]] = []
        app_proc_alive = False
        if session is not None:
            if session.proc is not None and session.proc.poll() is None:
                app_proc_alive = True
            problems.extend(session.stop())
            owned = list(session.owned)
            post_stop_survivors = living_survivors(owned)
        all_survivor_pids = {s.get("pid") for s in pre_stop_survivors}
        all_survivors = list(pre_stop_survivors)
        for s in post_stop_survivors:
            if s.get("pid") not in all_survivor_pids:
                all_survivors.append(s)
                all_survivor_pids.add(s.get("pid"))
        graceful_exit = bool(quit_ok and not app_proc_alive and not all_survivors and not problems)
        report["cleanup"] = {
            "checked": session is not None,
            "problems": problems,
            "survivors": all_survivors,
            "harnessForced": not graceful_exit,
            "productQuit": graceful_exit,
            "graceful": graceful_exit,
        }


def _write_report(out_dir: str, report: dict[str, Any]) -> str:
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, "native_resource_leaks.json")
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2, allow_nan=False)
        handle.write("\n")
    os.replace(temporary, path)
    return path


def exit_code(report: dict[str, Any]) -> int:
    if report.get("verdict") in ("SUBGATE_ACCEPTED", "LEAK_GATE_ACCEPTED"):
        return 0
    return 1


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Linux native resource-leak gate (native-resource-leaks).")
    parser.add_argument("--profile", choices=("short", "long"), default="short")
    parser.add_argument("--bin", default=None)
    parser.add_argument("--expected-binary-sha256", default=None)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--expected-fixture-sha256", default=None)
    parser.add_argument("--expected-source-sha", default=None)
    parser.add_argument("--build-receipt", default=None)
    parser.add_argument("--build-profile", default="unverified")
    parser.add_argument("--label", default="unlabeled")
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--measured-switches", type=int, default=None)
    parser.add_argument("--warmup-switches", type=int, default=None)
    parser.add_argument("--settle-seconds", type=float, default=None)
    parser.add_argument("--absolute-release-budget", action="store_true")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    out_dir = os.path.abspath(args.out_dir)
    if os.path.isdir(out_dir) and os.listdir(out_dir):
        print(f"refusing to write into non-empty {out_dir}", file=sys.stderr)
        return 2
    os.makedirs(out_dir, exist_ok=True)
    report = assemble_report(args)
    if args.profile == "long":
        # Hide and tray are not observable. Do not start the 600s soak.
        _add(report["reasons"], "missing-coverage")
    if report["reasons"]:
        report["driverStatus"] = "NOT_RUN"
        report = evaluate_report(report)
        path = _write_report(out_dir, report)
        print(f"{report['verdict']} {path}")
        print("reasons: " + ", ".join(report["reasons"]), file=sys.stderr)
        return exit_code(report)
    try:
        drive_product(report, out_dir)
    except Exception as exc:  # noqa: BLE001 - recorded, then the gate fails closed
        report["driverStatus"] = "FAILED"
        report["error"] = f"{type(exc).__name__}: {exc}"
        report["evidenceClass"] = "product-run-failed"
    measured = report.get("evidence", {}).get("measuredSwitches") or []
    endpoints = [row for row in report.get("samples") or [] if isinstance(row, dict) and row.get("phase") == "measured"]
    report["counts"]["checkpoints"] = len(endpoints)
    report["counts"]["quiescentCheckpoints"] = len(_measured_endpoints(report.get("samples") or []))
    report["counts"]["reposChecked"] = len({row.get("repo") for row in measured if isinstance(row, dict)})
    report = evaluate_report(report)
    path = _write_report(out_dir, report)
    print(f"{report['verdict']} {path}")
    if report["verdict"] == "NOT_ACCEPTED":
        print("reasons: " + ", ".join(report["reasons"]), file=sys.stderr)
    return exit_code(report)


if __name__ == "__main__":
    sys.exit(main())
