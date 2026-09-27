#!/usr/bin/env python3
"""Gate tests. Synthetic reports prove verdict math only, not a product or release pass."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
REPO_ROOT = os.path.dirname(SCRIPTS_DIR)
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from check_native_leaks import (  # noqa: E402
    NativeBenchError,
    STANDARD_BINARY_CANDIDATES,
    _log_supports,
    _track_app_descendants,
    _try_workspace_close_reopen,
    assemble_report,
    classify_workload,
    discover_standard_binary,
    evaluate_report,
    is_drained,
    living_survivors,
    main,
    phase_batches,
    planned_repo_sequence,
    source_sha_fact,
)
from bench_tauri_memory import sha256_file  # noqa: E402
import memory_harness  # noqa: E402
from memory_harness import ProcessTreeSampler, read_process_identity, sample_app_resources  # noqa: E402

MIB = 1024 * 1024
NAMES = [f"repo-{index:02d}" for index in range(1, 16)]
APP = "/tmp/synthetic-native-leak-app"
ROOT = {"pid": 4242, "starttime": 100, "exe": APP, "comm": "snip-desktop-native"}
DELTAS = [1, -1, 0.5, -0.5, 0, 1, -1, 0.5, -0.5, 0]


def _action(repo: str, phase: str = "measured") -> dict:
    return {
        "actionId": f"{phase}-{repo}-{phase}",
        "phase": phase,
        "repo": repo,
        "input": "click",
        "repoLoaded": True,
        "graphLoaded": True,
        "selectLine": f"[APP:REPO_SELECTING: 0 ({repo}) root=/tmp/{repo}]",
        "loadedLine": f"[APP:REPO_LOADED: {repo} files=1]",
        "graphLine": "[APP:GRAPH_LOADED: commits=1]",
        "clickToRepoLoadedMs": 12.5,
        "clickToGraphLoadedMs": 18.0,
        "oracle": {"ok": True, "sourceRows": 1, "reportedFiles": 1},
        "root": dict(ROOT),
    }


def _actions(total: int, phase: str) -> list[dict]:
    rows = []
    seen: dict[str, int] = {}
    for piece in phase_batches(NAMES, total):
        for name in piece:
            seen[name] = seen.get(name, 0) + 1
            row = _action(name, phase)
            row["actionId"] = f"{phase}-{name}-{seen[name]}"
            rows.append(row)
    return rows


def _view(repo: str = NAMES[0]) -> dict:
    return {
        "repoLoaded": True,
        "tab": "GitChanges",
        "selectLine": f"[APP:REPO_SELECTING: 0 ({repo}) root=/tmp/{repo}]",
        "loadedLine": f"[APP:REPO_LOADED: {repo} files=1]",
        "graphLine": "[APP:GRAPH_LOADED: commits=1]",
        "tabLine": "[APP:TAB_SWITCHED: GitChanges visible=true]",
    }


def _endpoint(index: int, done: int, rss: int, pss: int, fds: int = 40, threads: int = 12, watches: int = 4, **extra) -> dict:
    row = {
        "sampleId": f"s-{done}",
        "batchIndex": index,
        "warmup": False,
        "phase": "measured",
        "activity": "quiescent",
        "resourcesComplete": True,
        "memoryComplete": True,
        "measuredSwitchesCompleted": done,
        "settleSeconds": 3.0,
        "equivalentView": {"repo": NAMES[0], "tool": "GitChanges"},
        "viewEvidence": _view(),
        "rssBytes": rss,
        "pssBytes": pss,
        "fdCount": fds,
        "threadCount": threads,
        "watchCount": watches,
        "gitChildren": 0,
        "processCount": 1,
        "unreadablePids": [],
        "root": dict(ROOT),
        "gpuCombinedIntoRss": False,
        "vramBytes": None,
    }
    row.update(extra)
    return row


def _flat_endpoints(rss0: int = 200 * MIB, pss0: int = 120 * MIB) -> list[dict]:
    rows = []
    for index, done in enumerate(range(10, 101, 10)):
        delta = int(DELTAS[index] * MIB)
        rows.append(_endpoint(index, done, rss0 + delta, pss0 + delta))
    return rows


def stable_interactions() -> list[dict]:
    return [
        {"item": "tree", "ok": True, "input": "click", "log": "[APP:TREE_FILE_SELECTED: README.md]", "root": dict(ROOT)},
        {"item": "copy", "ok": True, "input": "click", "log": "[APP:COPY_DONE: copied=1]", "oracle": {"verified": True}, "root": dict(ROOT)},
        {"item": "paste", "ok": True, "input": "key", "log": "[APP:PASTE_PREVIEW: items=1]", "root": dict(ROOT)},
        {"item": "cancel", "ok": True, "input": "click", "log": "[APP:PASTE_CANCELLED]", "root": dict(ROOT)},
        {
            "item": "workspace-close-reopen",
            "ok": True,
            "input": "click",
            "control": "btn-close-workspace",
            "log": "[APP:WORKSPACE: state=closed] phase=drained intent=close-workspace jobs=0 inflight=0 queued=0 leaked=0 | [APP:WORKSPACE: state=open path=/ws] | [APP:READY_REPOS: 15]",
            "oracle": {
                "sameProcess": True,
                "clipboardPreserved": True,
                "drained": True,
                "reposReady": True,
            },
            "root": dict(ROOT),
        },
        {"item": "hide", "ok": False, "input": "click", "log": "", "reason": "no window-hide contract"},
        {"item": "tray", "ok": False, "input": "click", "log": "", "reason": "no tray contract"},
        {
            "item": "quit-cleanup",
            "ok": True,
            "input": "key",
            "control": "ctrl+q",
            "log": "[APP:QUIT: deferred]",
            "root": dict(ROOT),
        },
    ]


def stable_report() -> dict:
    return {
        "schemaVersion": 1,
        "check": "native-resource-leaks",
        "profile": "short",
        "evidenceClass": "synthetic",
        "driverStatus": "COMPLETED",
        "sourceSha": "a" * 40,
        "binarySha256": "b" * 64,
        "fixtureSha256": "c" * 64,
        "pins": {},
        "app": dict(ROOT),
        "binary": {"path": APP, "sha256": "b" * 64, "sha256After": "b" * 64},
        "sourceBuildAuthorized": False,
        "absoluteReleaseBudgetClaimed": False,
        "workload": {
            "class": "functional-15",
            "repoCount": 15,
            "repos": list(NAMES),
            "gitFactsMatch": True,
            "releaseWorkload": False,
        },
        "counts": {
            "warmupSwitchesRequested": 20,
            "measuredSwitchesRequested": 100,
            "settleSeconds": 3.0,
            "observationSeconds": 40.0,
        },
        "samples": _flat_endpoints(),
        "evidence": {"measuredSwitches": _actions(100, "measured"), "warmupSwitches": _actions(20, "warmup")},
        "interactions": stable_interactions(),
        "cleanup": {
            "checked": True, "problems": [], "survivors": [],
            "harnessForced": False, "productQuit": True, "graceful": True,
        },
        "reasons": [],
    }


class TestVerdictMath(unittest.TestCase):
    def test_stable_noise_accepts_subgate_only(self) -> None:
        report = evaluate_report(stable_report())
        self.assertEqual(report["verdict"], "SUBGATE_ACCEPTED", report["reasons"])
        self.assertFalse(report["productAcceptance"])
        self.assertFalse(report["releaseComplete"])
        self.assertEqual(report["d4"], "NOT_EVALUATED")
        self.assertEqual(report["releaseOverall"], "NOT_ACCEPTED")
        for item in ("hide", "tray"):
            self.assertIn(item, report["coverageGaps"])
        for item in ("workspace-close-reopen", "quit-cleanup"):
            self.assertNotIn(item, report["coverageGaps"])

    def test_changed_root_pid_is_rejected(self) -> None:
        report = stable_report()
        report["samples"][3]["root"] = {"pid": 9999, "starttime": 100, "exe": APP}
        report["app"] = dict(ROOT)
        result = evaluate_report(report)
        self.assertIn("identity-mismatch", result["reasons"])
        self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_incomplete_terminal_sample_is_not_filtered_away(self) -> None:
        report = stable_report()
        good = dict(report["samples"][2])
        good["sampleId"] = "early-good"
        bad = dict(report["samples"][2])
        bad.update({"sampleId": "late-bad", "resourcesComplete": False, "memoryComplete": False, "rssBytes": None, "activity": "busy", "gitChildren": 1})
        report["samples"][2] = good
        report["samples"].insert(3, bad)
        result = evaluate_report(report)
        self.assertTrue("incomplete-resources" in result["reasons"] or "busy-endpoint" in result["reasons"])
        self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_forced_quit_claim_is_rejected(self) -> None:
        report = stable_report()
        report["cleanup"] = {
            "checked": True, "problems": [], "survivors": [],
            "harnessForced": True, "productQuit": False, "graceful": False,
        }
        report["interactions"] = [{"item": "quit-cleanup", "ok": True, "input": "key", "log": "killed by harness"}]
        result = evaluate_report(report)
        self.assertIn("forced-quit-claimed", result["reasons"])
        self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_garbage_raw_and_product_relabel_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            raw = os.path.join(tmp, "raw_samples.jsonl")
            with open(raw, "w", encoding="utf-8") as handle:
                handle.write("this is not json\n" * 10)
            report = stable_report()
            report["evidenceClass"] = "product-run"
            report["rawSamples"] = {"path": raw}
            report["pins"] = {
                "binarySha256": report["binarySha256"],
                "fixtureSha256": report["fixtureSha256"],
                "sourceSha": report["sourceSha"],
            }
            result = evaluate_report(report)
            self.assertIn("raw-evidence", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_long_arbitrary_coverage_is_not_a_leak_gate_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            raw = os.path.join(tmp, "raw_samples.jsonl")
            with open(raw, "w", encoding="utf-8") as handle:
                handle.write("nope\n" * 10)
            report = stable_report()
            report["profile"] = "long"
            report["evidenceClass"] = "product-run"
            report["counts"]["measuredSwitchesRequested"] = 500
            report["counts"]["observationSeconds"] = 600
            report["evidence"]["measuredSwitches"] = _actions(100, "measured")
            report["rawSamples"] = {"path": raw}
            report["pins"] = {"binarySha256": report["binarySha256"], "fixtureSha256": report["fixtureSha256"], "sourceSha": report["sourceSha"]}
            report["workload"]["class"] = "standard-release"
            report["workload"]["gitFactsMatch"] = True
            report["interactions"] = [
                {"item": item, "ok": True, "input": "click", "log": "arbitrary"}
                for item in ("tree", "copy", "paste", "cancel", "hide", "tray", "workspace-close-reopen", "quit-cleanup")
            ]
            result = evaluate_report(report)
            self.assertNotEqual(result["verdict"], "LEAK_GATE_ACCEPTED")
            self.assertIn("raw-evidence", result["reasons"])
            self.assertFalse(result["productAcceptance"])

    def test_final_window_before_padding_is_rejected(self) -> None:
        report = stable_report()
        report["profile"] = "long"
        report["counts"]["measuredSwitchesRequested"] = 500
        report["counts"]["observationSeconds"] = 600
        report["workload"]["class"] = "standard-release"
        report["workload"]["gitFactsMatch"] = True
        rows = [{"sampleId": f"w{i}", "offsetSec": float(i * 3), "resourcesComplete": True, "rssBytes": 200 * MIB, "pssBytes": 120 * MIB} for i in range(10)]
        report["windows"] = {"baseline": {"seconds": 30, "samples": rows}, "final": {"seconds": 30, "samples": rows}}
        report["sampleOrder"] = [row["sampleId"] for row in rows] + ["after-final"]
        result = evaluate_report(report)
        self.assertIn("final-window-early", result["reasons"])
        self.assertNotEqual(result["verdict"], "LEAK_GATE_ACCEPTED")

    def test_one_final_sample_cannot_stand_for_the_window(self) -> None:
        report = stable_report()
        report["profile"] = "long"
        report["counts"]["observationSeconds"] = 600
        report["workload"]["class"] = "standard-release"
        report["workload"]["gitFactsMatch"] = True
        lonely = [{"sampleId": "only", "offsetSec": 0, "resourcesComplete": True, "rssBytes": 200 * MIB, "pssBytes": 120 * MIB}]
        report["windows"] = {"baseline": {"seconds": 30, "samples": lonely}, "final": {"seconds": 30, "samples": lonely}}
        report["sampleOrder"] = ["only"]
        result = evaluate_report(report)
        self.assertIn("undersized", result["reasons"])
        self.assertNotEqual(result["verdict"], "LEAK_GATE_ACCEPTED")

    def test_tiny_standard_preset_is_not_the_release_workload(self) -> None:
        kind = classify_workload({"preset": "standard", "summary": {"totalRepos": 15, "totalTrackedPaths": 1, "totalCommits": 1}})
        self.assertNotEqual(kind, "standard-release")

    def test_memory_fd_thread_and_watcher_growth_are_rejected(self) -> None:
        memory = stable_report()
        memory["samples"] = [_endpoint(i, done, 200 * MIB + int(0.5 * MIB * done), 100 * MIB + int(0.5 * MIB * done)) for i, done in enumerate(range(10, 101, 10))]
        self.assertTrue({"memory-growth", "memory-trend"} & set(evaluate_report(memory)["reasons"]))
        fds = stable_report()
        fds["samples"] = [_endpoint(i, done, 200 * MIB, 120 * MIB, fds=40 if i < 7 else 46) for i, done in enumerate(range(10, 101, 10))]
        self.assertIn("fd-growth", evaluate_report(fds)["reasons"])
        threads = stable_report()
        threads["samples"] = [_endpoint(i, done, 200 * MIB, 120 * MIB, threads=12 if i < 7 else 20) for i, done in enumerate(range(10, 101, 10))]
        self.assertIn("thread-growth", evaluate_report(threads)["reasons"])
        watches = stable_report()
        watches["samples"] = [_endpoint(i, done, 200 * MIB, 120 * MIB, watches=0 if i < 7 else 8) for i, done in enumerate(range(10, 101, 10))]
        self.assertIn("watcher-growth", evaluate_report(watches)["reasons"])

    def test_fd_growth_of_two_stays_inside_the_gate(self) -> None:
        report = stable_report()
        report["samples"] = [_endpoint(i, done, 200 * MIB, 120 * MIB, fds=40 if i < 7 else 42) for i, done in enumerate(range(10, 101, 10))]
        result = evaluate_report(report)
        self.assertNotIn("fd-growth", result["reasons"])
        self.assertEqual(result["verdict"], "SUBGATE_ACCEPTED", result["reasons"])

    def test_nan_noop_undersized_and_wrong_pins(self) -> None:
        nan = stable_report()
        nan["samples"][4]["rssBytes"] = float("nan")
        self.assertIn("nan-sample", evaluate_report(nan)["reasons"])
        noop = stable_report()
        noop["counts"]["measuredSwitchesActual"] = 100
        noop["evidence"]["measuredSwitches"] = []
        self.assertIn("noop-driver", evaluate_report(noop)["reasons"])
        short = stable_report()
        short["counts"]["measuredSwitchesRequested"] = 40
        short["evidence"]["measuredSwitches"] = _actions(40, "measured")
        short["samples"] = short["samples"][:4]
        self.assertIn("undersized", evaluate_report(short)["reasons"])
        wrong = stable_report()
        wrong["evidenceClass"] = "product-run"
        wrong["pins"] = {"binarySha256": "d" * 64, "fixtureSha256": wrong["fixtureSha256"], "sourceSha": wrong["sourceSha"]}
        self.assertIn("wrong-binary", evaluate_report(wrong)["reasons"])
        workload = stable_report()
        workload["workload"]["class"] = "other"
        workload["workload"]["repoCount"] = 2
        workload["workload"]["repos"] = ["a", "b"]
        self.assertIn("wrong-workload", evaluate_report(workload)["reasons"])

    def test_receipt_cannot_authorize_the_build(self) -> None:
        report = stable_report()
        report["sourceBuildAuthorized"] = True
        result = evaluate_report(report)
        self.assertIn("source-build-self-authorized", result["reasons"])
        self.assertFalse(result["sourceBuildAuthorized"])

    def test_thresholds_cannot_be_raised(self) -> None:
        report = stable_report()
        report["thresholds"] = dict(report.get("thresholds") or {})
        report["thresholds"] = {
            "memoryGrowthAbsBytes": 512 * MIB,
            "memoryGrowthFraction": 0.5,
            "memoryTrendBytesPerSwitch": 8 * MIB,
            "fdGrowthMax": 100,
            "threadGrowthMax": 100,
            "watcherGrowthMax": 50,
        }
        result = evaluate_report(report)
        self.assertIn("thresholds-raised", result["reasons"])
        self.assertEqual(result["thresholds"]["watcherGrowthMax"], 0)

    def test_warmup_covers_every_repo_and_sequence_is_global(self) -> None:
        sequence = planned_repo_sequence(NAMES, 100)
        self.assertEqual(set(sequence), set(NAMES))
        pieces = phase_batches(NAMES, 100)
        self.assertEqual([name for piece in pieces for name in piece], sequence)
        self.assertEqual(set(planned_repo_sequence(NAMES, 20)), set(NAMES))
        self.assertNotEqual(set(pieces[0]), set(NAMES))


class TestDiscovery(unittest.TestCase):
    def test_missing_candidates_fail_and_outside_paths_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            outsider = os.path.join(root, "snip-desktop-native")
            with open(outsider, "wb") as handle:
                handle.write(b"#!/bin/sh\n")
            os.chmod(outsider, 0o755)
            info = discover_standard_binary(root)
            self.assertFalse(info["ok"])
            self.assertEqual([os.path.relpath(path, root) for path in info["candidates"]], list(STANDARD_BINARY_CANDIDATES))

    def test_one_standard_candidate_is_found(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            path = os.path.join(root, "target", "debug", "snip-desktop-native")
            os.makedirs(os.path.dirname(path))
            with open(path, "wb") as handle:
                handle.write(b"#!/bin/sh\n")
            os.chmod(path, 0o755)
            info = discover_standard_binary(root)
            self.assertTrue(info["ok"])
            self.assertEqual(info["path"], os.path.realpath(path))


class TestLiveHelpers(unittest.TestCase):
    def test_sampler_rejects_real_memory_growth(self) -> None:
        code = (
            "import sys\n"
            "sys.stdout.write('ready\\n'); sys.stdout.flush()\n"
            "sys.stdin.readline()\n"
            "blob = bytearray(b'x' * (48 * 1024 * 1024))\n"
            "blob[0] = 1; blob[-1] = 2\n"
            "sys.stdout.write('grown\\n'); sys.stdout.flush()\n"
            "sys.stdin.readline()\n"
        )
        before, after, proc = _sample_around(code, "ready\n", "grown\n")
        try:
            self.assertGreater(after["totals"]["rssBytes"], before["totals"]["rssBytes"] + 32 * MIB)
            report = stable_report()
            report["evidenceClass"] = "synthetic-negative"
            report["samples"] = [
                _endpoint(i, done, (before if i < 7 else after)["totals"]["rssBytes"], (before if i < 7 else after)["totals"]["pssBytes"], fds=before["totals"]["fdCount"], threads=before["totals"]["threadCount"], watches=before["totals"]["watchCount"])
                for i, done in enumerate(range(10, 101, 10))
            ]
            result = evaluate_report(report)
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")
            self.assertTrue({"memory-growth", "memory-trend"} & set(result["reasons"]))
            self.assertFalse(result["productAcceptance"])
        finally:
            _kill_group(proc)

    def test_sampler_rejects_real_fd_and_thread_growth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            code = (
                "import os, sys\n"
                f"root = {directory!r}\n"
                "sys.stdout.write('ready\\n'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "handles = [open(os.path.join(root, f'f{i}'), 'w') for i in range(16)]\n"
                "sys.stdout.write('open\\n'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            )
            before, after, proc = _sample_around(code, "ready\n", "open\n")
            try:
                self.assertGreater(after["totals"]["fdCount"] - before["totals"]["fdCount"], 2)
                report = stable_report()
                report["samples"] = [
                    _endpoint(i, done, after["totals"]["rssBytes"], after["totals"]["pssBytes"], fds=before["totals"]["fdCount"] if i < 7 else after["totals"]["fdCount"], threads=before["totals"]["threadCount"], watches=before["totals"]["watchCount"] or 0)
                    for i, done in enumerate(range(10, 101, 10))
                ]
                self.assertIn("fd-growth", evaluate_report(report)["reasons"])
            finally:
                _kill_group(proc)

    def test_real_inotify_watch_growth_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            code = (
                "import ctypes, os, sys\n"
                "libc = ctypes.CDLL(None)\n"
                "fd = libc.inotify_init1(0)\n"
                "if fd < 0:\n"
                "    raise SystemExit('inotify_init failed')\n"
                "sys.stdout.write('ready\\n'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "for index in range(8):\n"
                "    path = os.path.join(sys.argv[1], str(index))\n"
                "    os.makedirs(path, exist_ok=True)\n"
                "    if libc.inotify_add_watch(fd, path.encode(), 2) < 0:\n"
                "        raise SystemExit('inotify_add_watch failed')\n"
                "sys.stdout.write('watched\\n'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            )
            proc = subprocess.Popen(
                [sys.executable, "-c", code, directory],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
            )
            try:
                assert proc.stdout is not None and proc.stdin is not None
                self.assertEqual(proc.stdout.readline().decode(), "ready\n")
                ident = read_process_identity(proc.pid)
                self.assertIsNotNone(ident)
                before = sample_app_resources(proc.pid, expected_exe=sys.executable, expected_starttime=ident["starttime"])
                proc.stdin.write(b"\n")
                proc.stdin.flush()
                self.assertEqual(proc.stdout.readline().decode(), "watched\n")
                after = sample_app_resources(proc.pid, expected_exe=sys.executable, expected_starttime=ident["starttime"])
                self.assertIsNotNone(before["totals"]["watchCount"])
                self.assertGreater(after["totals"]["watchCount"] - before["totals"]["watchCount"], 1)
                self.assertLessEqual(after["totals"]["fdCount"] - before["totals"]["fdCount"], 2)
                report = stable_report()
                report["samples"] = [
                    _endpoint(i, done, after["totals"]["rssBytes"], after["totals"]["pssBytes"], fds=before["totals"]["fdCount"], threads=before["totals"]["threadCount"], watches=before["totals"]["watchCount"] if i < 7 else after["totals"]["watchCount"])
                    for i, done in enumerate(range(10, 101, 10))
                ]
                result = evaluate_report(report)
                self.assertIn("watcher-growth", result["reasons"])
                self.assertNotIn("fd-growth", result["reasons"])
                self.assertFalse(result["productAcceptance"])
            finally:
                _kill_group(proc)

    def test_survivor_child_is_not_killed_by_the_scan(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(120)"],
            start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            ident = None
            for _ in range(50):
                ident = read_process_identity(proc.pid)
                if ident and ident.get("starttime") is not None:
                    break
            self.assertIsNotNone(ident)
            os.kill(proc.pid, 0)
            report = stable_report()
            report["cleanup"] = {"checked": True, "problems": [], "survivors": [ident], "harnessForced": True, "productQuit": False, "graceful": False}
            result = evaluate_report(report)
            self.assertIn("survivor-child", result["reasons"])
            os.kill(proc.pid, 0)
            self.assertEqual(living_survivors([ident])[0]["pid"], proc.pid)
        finally:
            _kill_group(proc)

    def test_unreadable_tree_evidence_is_not_a_complete_sample(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            exe = os.path.join(root, "bin", "app")
            os.makedirs(os.path.dirname(exe))
            with open(exe, "wb") as handle:
                handle.write(b"bin")
            _write_proc(root, 10, "app", 50, 20000, 15000, 4, 3, [99], exe, watches=0)
            snap = sample_app_resources(10, expected_exe=exe, expected_starttime=50, proc_root=root)
            self.assertFalse(snap["resourcesComplete"])
            self.assertIn(99, snap["unreadablePids"])
            self.assertIsNone(snap["totals"]["rssBytes"])
            self.assertIsNone(snap["totals"]["watchCount"])

    def test_missing_children_file_is_unknown_not_zero(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            exe = os.path.join(root, "bin", "app")
            os.makedirs(os.path.dirname(exe))
            with open(exe, "wb") as handle:
                handle.write(b"bin")
            _write_proc(root, 10, "app", 50, 20000, 15000, 4, 3, [], exe, watches=0, children_file=False)
            legacy = ProcessTreeSampler(10, None, root).get_tree_pids()
            self.assertEqual(legacy, ([10], []))
            snap = sample_app_resources(10, expected_exe=exe, expected_starttime=50, proc_root=root)
            self.assertFalse(snap["valid"])
            self.assertFalse(snap["resourcesComplete"])
            self.assertIn(10, snap["unreadablePids"])
            self.assertIsNone(snap["totals"]["gitChildren"])
            self.assertIsNone(snap["totals"]["fdCount"])
            self.assertIsNone(snap["totals"]["rssBytes"])
            self.assertNotEqual(snap["activity"], "quiescent")

    def test_child_identity_is_reread_after_fd_thread_and_watches(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            exe = os.path.join(root, "bin", "app")
            os.makedirs(os.path.dirname(exe))
            with open(exe, "wb") as handle:
                handle.write(b"bin")
            _write_proc(root, 10, "app", 50, 20000, 15000, 4, 3, [11], exe, watches=0)
            _write_proc(root, 11, "git", 70, 1000, 800, 1, 2, [], exe, watches=0)
            seen = {"fds": [], "threads": [], "watches": [], "child": 0, "root_after_fd": False}
            orig_ident = memory_harness.read_process_identity
            orig_fd = memory_harness.count_open_fds
            orig_threads = memory_harness.count_threads
            orig_watches = memory_harness.count_inotify_watches

            def fd(pid, proc_root="/proc"):
                seen["fds"].append(pid)
                return orig_fd(pid, proc_root)

            def threads(pid, proc_root="/proc"):
                seen["threads"].append(pid)
                return orig_threads(pid, proc_root)

            def watches(pid, proc_root="/proc"):
                seen["watches"].append(pid)
                return orig_watches(pid, proc_root)

            def ident(pid, proc_root="/proc"):
                row = orig_ident(pid, proc_root)
                if pid == 11 and row is not None:
                    seen["child"] += 1
                    if seen["child"] == 2:
                        seen["child_after_resources"] = (
                            11 in seen["fds"] and 11 in seen["threads"] and 11 in seen["watches"]
                        )
                        return {**row, "starttime": row["starttime"] + 5}
                if pid == 10 and 10 in seen["fds"]:
                    seen["root_after_fd"] = True
                return row

            memory_harness.read_process_identity = ident
            memory_harness.count_open_fds = fd
            memory_harness.count_threads = threads
            memory_harness.count_inotify_watches = watches
            try:
                snap = sample_app_resources(10, expected_exe=exe, expected_starttime=50, proc_root=root)
            finally:
                memory_harness.read_process_identity = orig_ident
                memory_harness.count_open_fds = orig_fd
                memory_harness.count_threads = orig_threads
                memory_harness.count_inotify_watches = orig_watches
            self.assertTrue(seen["child_after_resources"])
            self.assertTrue(seen["root_after_fd"])
            self.assertFalse(snap["resourcesComplete"])
            self.assertIn(11, snap["unreadablePids"])
            self.assertIsNone(snap["totals"]["gitChildren"])
            self.assertTrue(any("changed during sample" in reason for reason in snap["reasons"]))

    def test_deleted_executable_is_not_a_complete_sample(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            exe = os.path.join(root, "bin", "app")
            os.makedirs(os.path.dirname(exe))
            with open(exe, "wb") as handle:
                handle.write(b"bin")
            _write_proc(root, 10, "app", 50, 20000, 15000, 4, 3, [], exe, watches=0)
            link = os.path.join(root, "10", "exe")
            os.remove(link)
            os.symlink(exe + " (deleted)", link)
            snap = sample_app_resources(10, expected_exe=exe, expected_starttime=50, proc_root=root)
            self.assertFalse(snap["resourcesComplete"])
            self.assertIsNone(snap["totals"]["gitChildren"])
            self.assertTrue(any("deleted" in reason for reason in snap["reasons"]))


class TestCommandPreflight(unittest.TestCase):
    def test_unpinned_short_does_not_launch(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            binary, digest, workspace, fixture_sha = _mini_workspace(root)
            out = os.path.join(root, "out")
            code = main(["--profile", "short", "--bin", binary, "--workspace", workspace, "--out-dir", out])
            self.assertEqual(code, 1)
            with open(os.path.join(out, "native_resource_leaks.json"), encoding="utf-8") as handle:
                report = json.load(handle)
            self.assertEqual(report["driverStatus"], "NOT_RUN")
            self.assertIn("wrong-binary", report["reasons"])
            self.assertFalse(os.path.exists(os.path.join(out, "session")))
            self.assertNotIn(os.path.realpath(binary), report["binaryDiscovery"]["candidates"])
            self.assertTrue(digest and fixture_sha)

    def test_wrong_binary_sha_does_not_launch(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            binary, _digest, workspace, fixture_sha = _mini_workspace(root)
            out = os.path.join(root, "out")
            code = main([
                "--profile", "short", "--bin", binary, "--expected-binary-sha256", "a" * 64,
                "--workspace", workspace, "--expected-fixture-sha256", fixture_sha,
                "--expected-source-sha", source_sha_fact(REPO_ROOT) or "", "--out-dir", out,
            ])
            self.assertEqual(code, 1)
            with open(os.path.join(out, "native_resource_leaks.json"), encoding="utf-8") as handle:
                report = json.load(handle)
            self.assertIn("wrong-binary", report["reasons"])
            self.assertEqual(report["driverStatus"], "NOT_RUN")

    def test_long_does_not_launch_while_lifecycle_coverage_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            binary, digest, workspace, fixture_sha = _mini_workspace(root)
            out = os.path.join(root, "out")
            code = main([
                "--profile", "long", "--bin", binary, "--expected-binary-sha256", digest,
                "--workspace", workspace, "--expected-fixture-sha256", fixture_sha,
                "--expected-source-sha", source_sha_fact(REPO_ROOT) or "", "--out-dir", out,
            ])
            self.assertEqual(code, 1)
            self.assertFalse(os.path.exists(os.path.join(out, "session")))
            with open(os.path.join(out, "native_resource_leaks.json"), encoding="utf-8") as handle:
                report = json.load(handle)
            self.assertEqual(report["driverStatus"], "NOT_RUN")
            self.assertIn("missing-coverage", report["reasons"])


def _mini_workspace(root: str) -> tuple[str, str, str, str]:
    binary = os.path.join(root, "snip-desktop-native")
    with open(binary, "wb") as handle:
        handle.write(b"not-a-real-binary\n")
    os.chmod(binary, 0o755)
    workspace = os.path.join(root, "fixture")
    names = []
    for index in range(1, 16):
        name = f"repo-{index:02d}"
        os.makedirs(os.path.join(workspace, name))
        with open(os.path.join(workspace, name, ".git"), "w", encoding="utf-8") as handle:
            handle.write("gitdir: nowhere\n")
        names.append(name)
    manifest = {
        "preset": "smoke",
        "workloadRevision": "test",
        "seed": 1,
        "summary": {"totalRepos": 15, "totalCommits": 150, "totalTrackedPaths": 150, "totalRefs": 45},
        "repos": [{"name": name} for name in names],
    }
    path = os.path.join(workspace, "workload_manifest.json")
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle)
    return binary, sha256_file(binary), workspace, sha256_file(path)


def _spawn(code: str, args: list[str] | None = None) -> subprocess.Popen:
    return subprocess.Popen(
        [sys.executable, "-c", code, *(args or [])],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )


def _sample_around(code: str, first: str, second: str):
    proc = _spawn(code)
    assert proc.stdout is not None and proc.stdin is not None
    got = proc.stdout.readline().decode()
    if got != first:
        err = proc.stderr.read1(400) if proc.stderr else b""
        _kill_group(proc)
        raise AssertionError(f"helper said {got!r}, stderr {err!r}")
    ident = read_process_identity(proc.pid)
    assert ident is not None
    before = sample_app_resources(proc.pid, expected_exe=sys.executable, expected_starttime=ident["starttime"])
    proc.stdin.write(b"\n")
    proc.stdin.flush()
    got = proc.stdout.readline().decode()
    if got != second:
        _kill_group(proc)
        raise AssertionError(f"helper said {got!r}")
    after = sample_app_resources(proc.pid, expected_exe=sys.executable, expected_starttime=ident["starttime"])
    return before, after, proc


def _kill_group(proc: subprocess.Popen) -> None:
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=2)
    for stream in (proc.stdin, proc.stdout, proc.stderr):
        if stream is not None and not stream.closed:
            stream.close()


def _write_proc(root, pid, comm, start, rss_kb, pss_kb, threads, fds, children, exe, watches, children_file=True) -> None:
    directory = os.path.join(root, str(pid))
    os.makedirs(os.path.join(directory, "fd"), exist_ok=True)
    os.makedirs(os.path.join(directory, "fdinfo"), exist_ok=True)
    os.makedirs(os.path.join(directory, "task", str(pid)), exist_ok=True)
    if children_file:
        with open(os.path.join(directory, "task", str(pid), "children"), "w", encoding="utf-8") as handle:
            handle.write(" ".join(str(child) for child in children))
    fields = ["R", "1", "1", "1", "0", "-1", "0", "0", "0", "0", "0", "0", "0", "0", "0", "20", "0", str(threads), "0", str(start)]
    with open(os.path.join(directory, "stat"), "w", encoding="utf-8") as handle:
        handle.write(f"{pid} ({comm}) " + " ".join(fields) + "\n")
    with open(os.path.join(directory, "comm"), "w", encoding="utf-8") as handle:
        handle.write(comm + "\n")
    with open(os.path.join(directory, "status"), "w", encoding="utf-8") as handle:
        handle.write(f"Name:\t{comm}\nThreads:\t{threads}\n")
    with open(os.path.join(directory, "smaps_rollup"), "w", encoding="utf-8") as handle:
        handle.write(f"Rss: {rss_kb} kB\nPss: {pss_kb} kB\n")
    for index in range(fds):
        with open(os.path.join(directory, "fd", str(index)), "w", encoding="utf-8"):
            pass
        info = "pos:\t0\nflags:\t0\n"
        if watches:
            info += "".join(f"inotify wd:{n} ino:1 sdev:1 mask:fff ignored_mask:0 fhandle-bytes:0 fhandle-type:0\n" for n in range(watches))
        if watches is not None:
            with open(os.path.join(directory, "fdinfo", str(index)), "w", encoding="utf-8") as handle:
                handle.write(info)
    os.symlink(exe, os.path.join(directory, "exe"))


def _seal_sample(row: dict) -> dict:
    row["rssProvenance"] = "smaps_rollup"
    row["pssProvenance"] = "smaps_rollup"
    row["ownedTasks"] = None
    view = dict(row["equivalentView"])
    view.setdefault("workspaceOpen", True)
    view.setdefault("closeObserved", False)
    row["equivalentView"] = view
    return row


def _bundle(directory: str) -> tuple[dict, list[dict]]:
    report = stable_report()
    report["runId"] = "run-canonical"
    report["evidenceClass"] = "product-run"
    report["pins"] = {
        "binarySha256": report["binarySha256"],
        "fixtureSha256": report["fixtureSha256"],
        "sourceSha": report["sourceSha"],
    }
    report["binary"] = {"path": APP, "sha256": report["binarySha256"], "sha256After": report["binarySha256"]}
    for row in report["samples"]:
        _seal_sample(row)
    terminal = _endpoint(0, 100, report["samples"][-1]["rssBytes"], report["samples"][-1]["pssBytes"])
    terminal["phase"] = "terminal"
    terminal["sampleId"] = "terminal-final"
    terminal["batchIndex"] = -1
    _seal_sample(terminal)
    report["samples"].append(terminal)
    report["interactions"] = stable_interactions()
    payload = b"SNIP-LEAK-GATE-CANONICAL-CLIPBOARD-v1\n"
    clip = {"bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest(), "cleared": False}
    report["clipboard"] = {"baseline": dict(clip), "terminal": dict(clip)}
    report["rawSamples"] = {"path": os.path.join(directory, "raw_samples.jsonl")}
    rows: list[dict] = []

    def add(kind: str, body: dict) -> None:
        rows.append({"runId": report["runId"], "seq": len(rows), "tMono": float(len(rows)), "kind": kind, **body})

    add("clipboard", {"phase": "baseline", **clip})
    for action in report["evidence"]["warmupSwitches"]:
        add("action", action)
    for action in report["evidence"]["measuredSwitches"]:
        add("action", action)
    for sample in report["samples"]:
        if sample.get("phase") != "terminal":
            add("endpoint", sample)
    for interaction in report["interactions"]:
        if interaction.get("item") != "quit-cleanup":
            add("interaction", interaction)
    add("clipboard", {"phase": "terminal", **clip})
    add("terminal", terminal)
    for interaction in report["interactions"]:
        if interaction.get("item") == "quit-cleanup":
            add("interaction", interaction)
    return report, rows


def _reseq(rows: list[dict]) -> list[dict]:
    for index, row in enumerate(rows):
        row["seq"] = index
        row["tMono"] = float(index)
    return rows


def _write_rows(report: dict, rows: list[dict]) -> None:
    with open(report["rawSamples"]["path"], "w", encoding="utf-8") as handle:
        for row in rows:
            json.dump(row, handle)
            handle.write("\n")


def _judge(report: dict, rows: list[dict]) -> dict:
    _write_rows(report, rows)
    return evaluate_report(report)


def _set_exe(report: dict, rows: list[dict], exe: str) -> None:
    report["app"]["exe"] = exe
    for row in [*report["samples"], *report["evidence"]["measuredSwitches"], *report["evidence"]["warmupSwitches"], *report["interactions"]]:
        if isinstance(row, dict) and isinstance(row.get("root"), dict):
            row["root"]["exe"] = exe
    for row in rows:
        if isinstance(row.get("root"), dict):
            row["root"]["exe"] = exe


class TestCanonicalRaw(unittest.TestCase):
    def test_matching_raw_accepts_only_the_short_subgate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            result = _judge(report, rows)
            self.assertEqual(result["verdict"], "SUBGATE_ACCEPTED", result["reasons"])
            self.assertFalse(result["productAcceptance"])
            self.assertFalse(result["releaseComplete"])
            self.assertEqual(result["d4"], "NOT_EVALUATED")
            for item in ("hide", "tray"):
                self.assertIn(item, result["coverageGaps"])
            for item in ("workspace-close-reopen", "quit-cleanup"):
                self.assertNotIn(item, result["coverageGaps"])

    def test_missing_app_identity_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            report.pop("app")
            result = _judge(report, rows)
            self.assertIn("identity-mismatch", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_raw_resource_divergence_is_not_hidden_by_the_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for row in rows:
                if row["kind"] == "endpoint":
                    row["fdCount"] = 999
                    row["threadCount"] = 999
                    row["watchCount"] = 999
                    row["resourcesComplete"] = False
                    row["gitChildren"] = 1
            result = _judge(report, rows)
            self.assertIn("raw-divergent", result["reasons"])
            self.assertTrue({"incomplete-resources", "busy-endpoint", "fd-growth"} & set(result["reasons"]))
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_same_bad_raw_and_report_still_uses_the_resource_facts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for row in rows:
                if row["kind"] != "endpoint":
                    continue
                row["fdCount"] = 999
                row["resourcesComplete"] = False
                row["gitChildren"] = 1
                match = next(sample for sample in report["samples"] if sample["sampleId"] == row["sampleId"])
                match["fdCount"] = 999
                match["resourcesComplete"] = False
                match["gitChildren"] = 1
            result = _judge(report, rows)
            self.assertNotIn("raw-divergent", result["reasons"])
            self.assertTrue({"incomplete-resources", "busy-endpoint"} & set(result["reasons"]))
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_missing_warmup_raw_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            kept = [row for row in rows if not (row["kind"] == "action" and row["phase"] == "warmup")]
            result = _judge(report, _reseq(kept))
            self.assertIn("raw-divergent", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_sleep_executable_does_not_match_the_pinned_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            _set_exe(report, rows, "/bin/sleep")
            result = _judge(report, rows)
            self.assertIn("identity-mismatch", result["reasons"])
            self.assertNotIn("raw-divergent", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")
            self.assertEqual(report["binary"]["path"], APP)

    def test_deleted_executable_string_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            _set_exe(report, rows, APP + " (deleted)")
            result = _judge(report, rows)
            self.assertIn("identity-mismatch", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_different_equivalent_views_are_not_a_watcher_leak(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            endpoints = [row for row in rows if row["kind"] == "endpoint"]
            for index, row in enumerate(endpoints):
                repo = NAMES[index % len(NAMES)]
                evidence = _view(repo)
                view = {"repo": repo, "tool": "GitChanges", "workspaceOpen": True, "closeObserved": False}
                row["equivalentView"] = view
                row["viewEvidence"] = evidence
                row["watchCount"] = 0 if index < 7 else 8
                match = next(sample for sample in report["samples"] if sample["sampleId"] == row["sampleId"])
                match["equivalentView"] = view
                match["viewEvidence"] = evidence
                match["watchCount"] = row["watchCount"]
            result = _judge(report, rows)
            self.assertIn("equivalent-state", result["reasons"])
            self.assertNotIn("watcher-growth", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_open_close_state_must_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for row in rows:
                if row["kind"] == "terminal":
                    row["equivalentView"] = {**row["equivalentView"], "closeObserved": True}
            for sample in report["samples"]:
                if sample["sampleId"] == "terminal-final":
                    sample["equivalentView"] = {**sample["equivalentView"], "closeObserved": True}
            result = _judge(report, rows)
            self.assertIn("equivalent-state", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_late_fd_thread_watch_and_child_are_judged(self) -> None:
        cases = (
            ("fdCount", 40 + 10, "fd-growth"),
            ("threadCount", 12 + 8, "thread-growth"),
            ("watchCount", 4 + 1, "watcher-growth"),
        )
        for key, value, code in cases:
            with self.subTest(key=key):
                with tempfile.TemporaryDirectory() as tmp:
                    report, rows = _bundle(tmp)
                    for row in rows:
                        if row["kind"] == "terminal":
                            row[key] = value
                    for sample in report["samples"]:
                        if sample["sampleId"] == "terminal-final":
                            sample[key] = value
                    result = _judge(report, rows)
                    self.assertIn(code, result["reasons"])
                    self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for row in rows:
                if row["kind"] == "terminal":
                    row["gitChildren"] = 1
                    row["activity"] = "busy"
            for sample in report["samples"]:
                if sample["sampleId"] == "terminal-final":
                    sample["gitChildren"] = 1
                    sample["activity"] = "busy"
            result = _judge(report, rows)
            self.assertIn("busy-endpoint", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_post_checkpoint_spike_is_not_discarded(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            spike = copy.deepcopy(next(row for row in rows if row["kind"] == "terminal"))
            spike["kind"] = "extension"
            spike["sampleId"] = "late-spike"
            spike["fdCount"] = 999
            spike["settleSeconds"] = 0.2
            terminal_at = next(index for index, row in enumerate(rows) if row["kind"] == "terminal")
            rows.insert(terminal_at, {key: value for key, value in spike.items() if key not in ("seq", "tMono", "runId")})
            rows[terminal_at]["runId"] = report["runId"]
            report["samples"].insert(-1, {key: value for key, value in spike.items() if key not in ("runId", "seq", "tMono", "kind")})
            result = _judge(report, _reseq(rows))
            self.assertIn("fd-growth", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_interaction_after_terminal_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            extra = {"item": "tree", "ok": True, "input": "click", "log": "[APP:TREE_FILE_SELECTED: later]", "root": dict(ROOT)}
            rows.append({"runId": report["runId"], "kind": "interaction", **extra})
            report["interactions"].append(extra)
            result = _judge(report, _reseq(rows))
            self.assertIn("final-window-early", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_duplicate_clean_sample_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            duplicate = copy.deepcopy(next(row for row in rows if row["kind"] == "endpoint"))
            rows.append(duplicate)
            result = _judge(report, _reseq(rows))
            self.assertIn("raw-evidence", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_missing_fd_field_and_wrong_run_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            broken = copy.deepcopy(rows)
            next(row for row in broken if row["kind"] == "endpoint").pop("fdCount")
            missing = _judge(report, broken)
            self.assertIn("raw-evidence", missing["reasons"])
            self.assertNotEqual(missing["verdict"], "SUBGATE_ACCEPTED")
            wrong = copy.deepcopy(rows)
            wrong[-1]["runId"] = "other-run"
            result = _judge(report, wrong)
            self.assertIn("raw-evidence", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_null_git_children_is_incomplete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for row in rows:
                if row["kind"] == "terminal":
                    row["gitChildren"] = None
            for sample in report["samples"]:
                if sample["sampleId"] == "terminal-final":
                    sample["gitChildren"] = None
            result = _judge(report, rows)
            self.assertIn("incomplete-resources", result["reasons"])
            self.assertNotEqual(result["verdict"], "SUBGATE_ACCEPTED")

    def test_clipboard_must_match_or_be_attributed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            empty = hashlib.sha256(b"").hexdigest()
            for side in (report["clipboard"]["terminal"], next(row for row in rows if row["kind"] == "clipboard" and row["phase"] == "terminal")):
                side["bytes"] = 0
                side["sha256"] = empty
                side["cleared"] = True
            cleared = _judge(report, rows)
            self.assertIn("clipboard-state", cleared["reasons"])
            self.assertNotEqual(cleared["verdict"], "SUBGATE_ACCEPTED")
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            other = hashlib.sha256(b"different").hexdigest()
            for side in (report["clipboard"]["terminal"], next(row for row in rows if row["kind"] == "clipboard" and row["phase"] == "terminal")):
                side["bytes"] = len(b"different")
                side["sha256"] = other
            diverged = _judge(report, rows)
            self.assertIn("clipboard-state", diverged["reasons"])
            self.assertNotEqual(diverged["verdict"], "SUBGATE_ACCEPTED")
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            other = hashlib.sha256(b"copied-file").hexdigest()
            attribution = {"legitimate": True, "source": "explicit-copy", "sha256": other, "bytes": len(b"copied-file")}
            for side in (report["clipboard"]["terminal"], next(row for row in rows if row["kind"] == "clipboard" and row["phase"] == "terminal")):
                side["bytes"] = len(b"copied-file")
                side["sha256"] = other
                side["attribution"] = attribution
            kept = _judge(report, rows)
            self.assertNotIn("clipboard-state", kept["reasons"])
            self.assertEqual(kept["verdict"], "SUBGATE_ACCEPTED", kept["reasons"])

    def test_fake_task_and_gpu_zeros_are_rejected(self) -> None:
        tasks = stable_report()
        tasks["samples"][0]["ownedTasks"] = 0
        self.assertIn("fake-tasks", evaluate_report(tasks)["reasons"])
        gpu = stable_report()
        gpu["samples"][0]["vramBytes"] = 0
        self.assertIn("gpu-combined", evaluate_report(gpu)["reasons"])

    def test_long_final_window_counts_resources_and_trailing_actions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            report["profile"] = "long"
            report["counts"]["measuredSwitchesRequested"] = 500
            report["counts"]["observationSeconds"] = 600
            report["workload"]["class"] = "standard-release"
            report["workload"]["gitFactsMatch"] = True

            def window(kind: str, fd: int) -> list[dict]:
                made = []
                for index in range(10):
                    sample = _seal_sample(_endpoint(0, 100, 200 * MIB, 120 * MIB, fds=fd))
                    sample["sampleId"] = f"{kind}-{index}"
                    sample["phase"] = kind
                    sample["offsetSec"] = index * (30 / 9)
                    made.append(sample)
                return made

            baseline = window("baseline", 40)
            final = window("final", 40)
            final[-1]["fdCount"] = 50
            report["windows"] = {
                "baseline": {"seconds": 30, "samples": baseline},
                "final": {"seconds": 30, "samples": final},
            }
            # Drop the short terminal so the final window can be the suffix.
            rows = [row for row in rows if row["kind"] != "terminal"]
            report["samples"] = [sample for sample in report["samples"] if sample.get("phase") != "terminal"]
            _reseq(rows)
            for sample in baseline + final:
                rows.append({"runId": report["runId"], "kind": sample["phase"], **sample})
            rows.append({
                "runId": report["runId"], "kind": "interaction",
                "item": "tree", "ok": True, "input": "click",
                "log": "[APP:TREE_FILE_SELECTED: after-final]", "root": dict(ROOT),
            })
            report["interactions"].append({
                "item": "tree", "ok": True, "input": "click",
                "log": "[APP:TREE_FILE_SELECTED: after-final]", "root": dict(ROOT),
            })
            result = _judge(report, _reseq(rows))
            self.assertIn("fd-growth", result["reasons"])
            self.assertIn("final-window-early", result["reasons"])
            self.assertNotEqual(result["verdict"], "LEAK_GATE_ACCEPTED")


class TestGateCorrectionsAndRegressions(unittest.TestCase):
    def test_missing_pss_in_long_window_returns_controlled_not_accepted(self) -> None:
        report = stable_report()
        report["profile"] = "long"
        report["counts"]["observationSeconds"] = 600
        report["counts"]["measuredSwitchesRequested"] = 500
        report["workload"]["class"] = "standard-release"
        report["workload"]["gitFactsMatch"] = True
        rows = [
            {
                "sampleId": f"w{i}",
                "offsetSec": float(i * 3.1),
                "resourcesComplete": True,
                "memoryComplete": True,
                "rssBytes": 200 * MIB,
                "pssBytes": None if i == 5 else 120 * MIB,
                "fdCount": 40,
                "threadCount": 12,
                "watchCount": 4,
                "gitChildren": 0,
            }
            for i in range(10)
        ]
        report["windows"] = {
            "baseline": {"seconds": 30.0, "samples": rows},
            "final": {"seconds": 30.0, "samples": copy.deepcopy(rows)},
        }
        report["sampleOrder"] = [r["sampleId"] for r in rows]
        result = evaluate_report(report)
        self.assertIn("missing-samples", result["reasons"])
        self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_nonfinite_metrics_in_long_window_returns_controlled_not_accepted(self) -> None:
        report = stable_report()
        report["profile"] = "long"
        report["counts"]["observationSeconds"] = 600
        report["counts"]["measuredSwitchesRequested"] = 500
        report["workload"]["class"] = "standard-release"
        report["workload"]["gitFactsMatch"] = True
        rows = [
            {
                "sampleId": f"w{i}",
                "offsetSec": float(i * 3.1),
                "resourcesComplete": True,
                "memoryComplete": True,
                "rssBytes": float("nan") if i == 2 else 200 * MIB,
                "pssBytes": float("inf") if i == 4 else 120 * MIB,
                "fdCount": float("nan") if i == 6 else 40,
                "threadCount": 12,
                "watchCount": 4,
                "gitChildren": 0,
            }
            for i in range(10)
        ]
        report["windows"] = {
            "baseline": {"seconds": 30.0, "samples": rows},
            "final": {"seconds": 30.0, "samples": copy.deepcopy(rows)},
        }
        report["sampleOrder"] = [r["sampleId"] for r in rows]
        result = evaluate_report(report)
        self.assertTrue("missing-samples" in result["reasons"] or "incomplete-resources" in result["reasons"])
        self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_product_run_missing_or_none_sha256_after_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            report["binary"]["sha256After"] = None
            result = _judge(report, rows)
            self.assertIn("wrong-binary", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

            report2, rows2 = _bundle(tmp)
            report2["binary"].pop("sha256After", None)
            result2 = _judge(report2, rows2)
            self.assertIn("wrong-binary", result2["reasons"])
            self.assertEqual(result2["verdict"], "NOT_ACCEPTED")

    def test_transient_settling_sample_does_not_fail_if_terminal_settled(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            terminal_idx = next(i for i, r in enumerate(rows) if r["kind"] == "terminal")
            transient = copy.deepcopy(rows[terminal_idx])
            transient["kind"] = "settle-point"
            transient["sampleId"] = "transient-settle-1"
            transient["gitChildren"] = 1
            transient["settleSeconds"] = 0.5
            rows.insert(terminal_idx, {k: v for k, v in transient.items() if k not in ("seq", "tMono")})
            report["samples"].insert(-1, {k: v for k, v in transient.items() if k not in ("runId", "seq", "tMono", "kind")})
            result = _judge(report, _reseq(rows))
            self.assertNotIn("busy-endpoint", result["reasons"])
            self.assertEqual(result["verdict"], "SUBGATE_ACCEPTED", result["reasons"])

    def test_busy_final_terminal_endpoint_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            terminal_idx = next(i for i, r in enumerate(rows) if r["kind"] == "terminal")
            rows[terminal_idx]["gitChildren"] = 1
            report["samples"][-1]["gitChildren"] = 1
            result = _judge(report, _reseq(rows))
            self.assertIn("busy-endpoint", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_incomplete_final_terminal_endpoint_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            terminal_idx = next(i for i, r in enumerate(rows) if r["kind"] == "terminal")
            rows[terminal_idx]["resourcesComplete"] = False
            report["samples"][-1]["resourcesComplete"] = False
            result = _judge(report, _reseq(rows))
            self.assertIn("incomplete-resources", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_failing_copy_paste_cancel_fails_short_coverage(self) -> None:
        for item in ("tree", "copy", "paste", "cancel"):
            with tempfile.TemporaryDirectory() as tmp:
                report, rows = _bundle(tmp)
                for inter in report["interactions"]:
                    if inter.get("item") == item:
                        inter["ok"] = False
                for row in rows:
                    if row.get("kind") == "interaction" and row.get("item") == item:
                        row["ok"] = False
                result = _judge(report, _reseq(rows))
                self.assertIn("missing-coverage", result["reasons"])
                self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_failed_or_missing_workspace_close_reopen_fails_short_coverage(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for inter in report["interactions"]:
                if inter.get("item") == "workspace-close-reopen":
                    inter["ok"] = False
            for row in rows:
                if row.get("kind") == "interaction" and row.get("item") == "workspace-close-reopen":
                    row["ok"] = False
            result = _judge(report, _reseq(rows))
            self.assertIn("missing-coverage", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            report["interactions"] = [i for i in report["interactions"] if i.get("item") != "workspace-close-reopen"]
            rows = [r for r in rows if not (r.get("kind") == "interaction" and r.get("item") == "workspace-close-reopen")]
            result = _judge(report, _reseq(rows))
            self.assertIn("missing-coverage", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            for inter in report["interactions"]:
                if inter.get("item") == "workspace-close-reopen":
                    inter["oracle"]["clipboardPreserved"] = False
            for row in rows:
                if row.get("kind") == "interaction" and row.get("item") == "workspace-close-reopen":
                    row["oracle"]["clipboardPreserved"] = False
            result = _judge(report, _reseq(rows))
            self.assertIn("missing-coverage", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_harness_forced_quit_fails_short_coverage(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            report["cleanup"]["harnessForced"] = True
            report["cleanup"]["productQuit"] = False
            report["cleanup"]["graceful"] = False
            result = _judge(report, rows)
            self.assertTrue("forced-quit-claimed" in result["reasons"] or "missing-coverage" in result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_build_receipt_source_sha_matches_expected_source(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_path = os.path.join(tmp, "fake-bin")
            with open(bin_path, "wb") as f:
                f.write(b"ELF-fake-binary")
            os.chmod(bin_path, 0o755)
            bin_sha = hashlib.sha256(b"ELF-fake-binary").hexdigest()
            receipt_path = os.path.join(tmp, "receipt.json")
            receipt_data = {
                "sha256": bin_sha,
                "sourceSha": "f" * 40,
            }
            with open(receipt_path, "w", encoding="utf-8") as f:
                json.dump(receipt_data, f)
            ws_dir = os.path.join(tmp, "ws")
            os.makedirs(ws_dir)

            class Args:
                bin = bin_path
                workspace = ws_dir
                profile = "short"
                expected_binary_sha256 = bin_sha
                expected_fixture_sha256 = None
                expected_source_sha = "f" * 40
                build_receipt = receipt_path
                build_profile = "release"
                label = "test"
                measured_switches = 100
                warmup_switches = 20
                settle_seconds = 3.0
                absolute_release_budget = False

            report = assemble_report(Args())
            self.assertEqual(report["sourceSha"], "f" * 40)
            self.assertEqual(report["driverSourceSha"], source_sha_fact(REPO_ROOT))
            self.assertNotIn("wrong-revision", report["reasons"])

            Args.expected_source_sha = "e" * 40
            report_bad = assemble_report(Args())
            self.assertIn("wrong-revision", report_bad["reasons"])

    def test_long_profile_truthfully_returns_not_accepted_for_unsupported_scope(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = os.path.join(tmp, "out")
            ws_dir = os.path.join(tmp, "ws")
            os.makedirs(ws_dir)
            code = main([
                "--profile", "long",
                "--workspace", ws_dir,
                "--out-dir", out_dir,
                "--expected-source-sha", "a" * 40,
            ])
            self.assertEqual(code, 1)
            report_path = os.path.join(out_dir, "native_resource_leaks.json")
            self.assertTrue(os.path.isfile(report_path))
            with open(report_path, encoding="utf-8") as handle:
                rep = json.load(handle)
            self.assertEqual(rep["verdict"], "NOT_ACCEPTED")
            self.assertIn("missing-coverage", rep["reasons"])
            self.assertFalse(rep["productAcceptance"])
            self.assertFalse(rep["releaseComplete"])

    def test_realistic_driver_order_with_quit_after_terminal_is_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            term_idx = next(i for i, r in enumerate(rows) if r.get("kind") == "terminal")
            quit_idx = next(i for i, r in enumerate(rows) if r.get("kind") == "interaction" and r.get("item") == "quit-cleanup")
            self.assertGreater(quit_idx, term_idx)
            result = _judge(report, rows)
            self.assertEqual(result["verdict"], "SUBGATE_ACCEPTED", result["reasons"])
            self.assertNotIn("final-window-early", result["reasons"])

    def test_workload_action_after_terminal_is_rejected_as_final_window_early(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            late_action = copy.deepcopy(rows[1])
            late_action["actionId"] = "measured-late-action"
            late_action["phase"] = "measured"
            late_action["seq"] = len(rows)
            late_action["tMono"] = float(len(rows))
            rows.append(late_action)
            report["evidence"]["measuredSwitches"].append(late_action)
            result = _judge(report, rows)
            self.assertIn("final-window-early", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_workload_interaction_after_terminal_is_rejected_as_final_window_early(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            late_interaction = {
                "kind": "interaction",
                "item": "tree",
                "ok": True,
                "input": "click",
                "log": "[APP:TREE_FILE_SELECTED: late.txt]",
                "root": dict(ROOT),
            }
            rows.append({"runId": report["runId"], "seq": len(rows), "tMono": float(len(rows)), **late_interaction})
            report["interactions"].append(late_interaction)
            result = _judge(report, rows)
            self.assertIn("final-window-early", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_multiple_quits_after_terminal_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report, rows = _bundle(tmp)
            extra_quit = {
                "kind": "interaction",
                "item": "quit-cleanup",
                "ok": True,
                "input": "key",
                "control": "ctrl+q",
                "log": "[APP:QUIT: deferred]",
                "root": dict(ROOT),
            }
            rows.append({"runId": report["runId"], "seq": len(rows), "tMono": float(len(rows)), **extra_quit})
            report["interactions"].append(extra_quit)
            result = _judge(report, rows)
            self.assertIn("final-window-early", result["reasons"])
            self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_product_quit_without_valid_quit_log_fails_forced_quit_claimed(self) -> None:
        report = stable_report()
        report["cleanup"] = {
            "checked": True, "problems": [], "survivors": [],
            "harnessForced": False, "productQuit": True, "graceful": True,
        }
        for row in report["interactions"]:
            if row.get("item") == "quit-cleanup":
                row["log"] = "killed by user without log"
        result = evaluate_report(report)
        self.assertIn("forced-quit-claimed", result["reasons"])
        self.assertEqual(result["verdict"], "NOT_ACCEPTED")

    def test_close_reopen_requires_zero_app_descendants_before_reopen(self) -> None:
        win = {"wid": "0x1234"}
        note_calls = []

        def mock_note(**kwargs):
            note_calls.append(kwargs)

        def make_session():
            s = mock.MagicMock()
            s.lines = [
                "[APP:CTRL_BOUNDS: id=btn-workspace-menu x=10 y=10 w=20 h=20]",
                "[APP:CTRL_BOUNDS: id=btn-close-workspace x=10 y=30 w=20 h=20]",
                "[APP:CTRL_BOUNDS: id=btn-open-workspace x=10 y=50 w=20 h=20]",
                "[APP:CTRL_BOUNDS: id=workspace-path-input x=10 y=70 w=100 h=20]",
                "[APP:CTRL_BOUNDS: id=btn-workspace-open-confirm x=10 y=90 w=20 h=20]",
                "[APP:CTRL_BOUNDS: id=workspace-closed x=10 y=110 w=100 h=100]",
            ]
            s.texts = mock.MagicMock(side_effect=lambda start=0: list(s.lines[start:]))
            s.app = {"pid": 1000, "starttime": 50, "exe": "/bin/snip", "comm": "snip"}
            s.proc = mock.MagicMock()
            s.proc.pid = 900
            s.xvfb = mock.MagicMock()
            s.xvfb.pid = 800
            s.app_tree_pids = mock.MagicMock(return_value=[1000])

            def mock_click(win_arg, bounds):
                if tuple(bounds) == (10, 30, 20, 20):  # btn-close-workspace
                    s.lines.extend([
                        "[APP:WORKSPACE: state=closed generation=1]",
                        "phase=drained intent=close-workspace jobs=0 inflight=0 queued=0 leaked=0 generation=1",
                    ])
                elif tuple(bounds) == (10, 90, 20, 20):  # btn-workspace-open-confirm
                    s.lines.extend([
                        "[APP:WORKSPACE: state=open path=/tmp/ws generation=2]",
                        "[APP:READY_REPOS: 15]",
                    ])
            s.click = mock.MagicMock(side_effect=mock_click)

            def mock_wait_line(pred, start=0, timeout=10.0):
                for line in s.lines[start:]:
                    if pred(line):
                        return (0, 0.0, line)
                raise NativeBenchError(f"line not found starting from {start}")
            s.wait_line = mock.MagicMock(side_effect=mock_wait_line)
            return s

        with mock.patch("check_native_leaks._read_clip", return_value={"sha256": "payload_hash", "bytes": 100, "cleared": False}), \
             mock.patch("check_native_leaks.descendants", return_value=[]), \
             mock.patch("check_native_leaks.is_same_process") as mock_same_proc:
            # Case A: app work descendant is alive during close
            session_a = make_session()
            mock_same_proc.side_effect = lambda ident: True
            tracked_descendants = {
                (2000, 100): {"pid": 2000, "starttime": 100, "exe": "/bin/git", "comm": "git"}
            }
            _try_workspace_close_reopen(session_a, win, "/tmp/ws", 15, tracked_descendants, mock_note)
            self.assertEqual(len(note_calls), 1)
            self.assertFalse(note_calls[0]["ok"])
            self.assertFalse(note_calls[0]["oracle"]["drained"])

            # Case B: app work descendant terminated (zero living descendants)
            note_calls.clear()
            session_b = make_session()
            mock_same_proc.side_effect = lambda ident: ident.get("pid") == 1000
            _try_workspace_close_reopen(session_b, win, "/tmp/ws", 15, tracked_descendants, mock_note)
            self.assertEqual(len(note_calls), 1)
            self.assertTrue(note_calls[0]["ok"])
            self.assertTrue(note_calls[0]["oracle"]["drained"])

    def test_close_reopen_rejects_stale_startup_ready_repos(self) -> None:
        session = mock.MagicMock()
        win = {"wid": "0x1234"}
        note_calls = []

        def mock_note(**kwargs):
            note_calls.append(kwargs)

        startup_lines = [
            "[APP:READY_REPOS: 15]",
            "[APP:CTRL_BOUNDS: id=btn-workspace-menu x=10 y=10 w=20 h=20]",
            "[APP:CTRL_BOUNDS: id=btn-close-workspace x=10 y=30 w=20 h=20]",
            "[APP:CTRL_BOUNDS: id=btn-open-workspace x=10 y=50 w=20 h=20]",
            "[APP:CTRL_BOUNDS: id=workspace-path-input x=10 y=70 w=100 h=20]",
            "[APP:CTRL_BOUNDS: id=btn-workspace-open-confirm x=10 y=90 w=20 h=20]",
            "[APP:CTRL_BOUNDS: id=workspace-closed x=10 y=110 w=100 h=100]",
        ]
        session.lines = list(startup_lines)
        session.texts = mock.MagicMock(side_effect=lambda start=0: list(session.lines[start:]))
        session.app = {"pid": 1000, "starttime": 50, "exe": "/bin/snip", "comm": "snip"}
        session.proc = mock.MagicMock()
        session.proc.pid = 900
        session.xvfb = mock.MagicMock()
        session.xvfb.pid = 800
        session.app_tree_pids = mock.MagicMock(return_value=[1000])

        def mock_click(win_arg, bounds):
            if tuple(bounds) == (10, 30, 20, 20):  # btn-close-workspace
                session.lines.extend([
                    "[APP:WORKSPACE: state=closed generation=1]",
                    "phase=drained intent=close-workspace jobs=0 inflight=0 queued=0 leaked=0 generation=1",
                ])
            elif tuple(bounds) == (10, 90, 20, 20):  # btn-workspace-open-confirm
                # Fresh state=open, but NO fresh READY_REPOS line!
                session.lines.extend([
                    "[APP:WORKSPACE: state=open path=/tmp/ws generation=2]",
                ])
        session.click = mock.MagicMock(side_effect=mock_click)

        def mock_wait_line(pred, start=0, timeout=10.0):
            for line in session.lines[start:]:
                if pred(line):
                    return (0, 0.0, line)
            raise NativeBenchError(f"line not found starting from {start}")

        session.wait_line = mock.MagicMock(side_effect=mock_wait_line)

        with mock.patch("check_native_leaks._read_clip", return_value={"sha256": "payload_hash", "bytes": 100, "cleared": False}), \
             mock.patch("check_native_leaks.descendants", return_value=[]), \
             mock.patch("check_native_leaks.is_same_process", return_value=True):
            tracked_descendants = {}
            _try_workspace_close_reopen(session, win, "/tmp/ws", 15, tracked_descendants, mock_note)
            self.assertEqual(len(note_calls), 1)
            self.assertFalse(note_calls[0]["ok"])
            self.assertIn("line not found", note_calls[0]["reason"])

    def test_graceful_quit_rejects_leaked_child_even_if_reaped_by_stop(self) -> None:
        report = stable_report()
        leaked_child = {"pid": 7777, "starttime": 88, "exe": "/bin/git", "comm": "git"}
        report["cleanup"] = {
            "checked": True,
            "problems": ["owned git pid 7777 survived teardown for 5.0s; sent SIGKILL"],
            "survivors": [leaked_child],
            "harnessForced": True,
            "productQuit": False,
            "graceful": False,
        }
        result = evaluate_report(report)
        self.assertIn("survivor-child", result["reasons"])
        self.assertIn("forced-quit-claimed", result["reasons"])
        self.assertIn("quit-cleanup", result["coverageGaps"])
        self.assertEqual(result["verdict"], "NOT_ACCEPTED")


if __name__ == "__main__":
    unittest.main()
