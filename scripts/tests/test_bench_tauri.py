#!/usr/bin/env python3
"""Contracts for the Tauri baseline driver and the attach mode it relies on.

1. App PID comes only from the driver's own descendants, by exact exe; duplicates fail.
2. A null, empty or different preview fails against the git oracle.
3. The git oracle reads a real commit, skips binaries and merges.
4. Attach mode: PID reuse, the process dying mid-steady, a stale or oversized ready file all fail.
"""

from __future__ import annotations

import os
import hashlib
import subprocess
import sys
import tempfile
import threading
import time
import unittest

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

import json  # noqa: E402
import signal  # noqa: E402
from unittest import mock  # noqa: E402

import bench_tauri_memory  # noqa: E402
from bench_tauri_memory import (  # noqa: E402
    BenchError,
    MATCHED_REF,
    MATCHED_SENTINEL,
    check_matched_measurement,
    check_matched_diff,
    check_matched_state,
    check_source_content,
    commit_oracle,
    find_owned_app_pid,
    identity,
    missing_diff_lines,
    matched_identity,
    matched_oracle,
    matched_tauri_state,
    reap_owned,
    summarize,
)
from memory_harness import (  # noqa: E402
    HarnessError,
    ProcessCrashedError,
    ReadinessTimeoutError,
    measure_single_profile,
    read_proc_starttime,
)


def fake_proc(root: str, pid: int, exe: str, children: list[int]) -> None:
    task = os.path.join(root, str(pid), "task", str(pid))
    os.makedirs(task)
    with open(os.path.join(task, "children"), "w") as f:
        f.write(" ".join(map(str, children)))
    os.symlink(exe, os.path.join(root, str(pid), "exe"))


class TestOwnedPid(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.proc = os.path.join(self.tmp.name, "proc")
        self.bin = os.path.join(self.tmp.name, "snip-sync")
        open(self.bin, "w").close()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_only_owned_descendant_is_selected(self) -> None:
        # 10 xvfb-run -> 11 tauri-driver -> 12 WebKitWebDriver -> 13 app -> 14 WebKitWebProcess
        fake_proc(self.proc, 10, "/bin/sh", [11])
        fake_proc(self.proc, 11, "/usr/bin/tauri-driver", [12])
        fake_proc(self.proc, 12, "/usr/bin/WebKitWebDriver", [13])
        fake_proc(self.proc, 13, self.bin, [14])
        fake_proc(self.proc, 14, "/usr/lib/WebKitWebProcess", [])
        # The user's own snip-sync, not ours; lower PID so a /proc scan would pick it first.
        fake_proc(self.proc, 5, self.bin, [])
        self.assertEqual(find_owned_app_pid(10, self.bin, self.proc), 13)

    def test_not_started_yet_is_none_without_fallback(self) -> None:
        fake_proc(self.proc, 10, "/bin/sh", [11])
        fake_proc(self.proc, 11, "/usr/bin/tauri-driver", [])
        fake_proc(self.proc, 5, self.bin, [])
        self.assertIsNone(find_owned_app_pid(10, self.bin, self.proc))

    def test_basename_lookalike_is_not_matched(self) -> None:
        other = os.path.join(self.tmp.name, "other", "snip-sync")
        os.makedirs(os.path.dirname(other))
        open(other, "w").close()
        fake_proc(self.proc, 10, "/bin/sh", [11])
        fake_proc(self.proc, 11, other, [])
        self.assertIsNone(find_owned_app_pid(10, self.bin, self.proc))

    def test_two_owned_instances_fail(self) -> None:
        fake_proc(self.proc, 10, "/bin/sh", [11, 12])
        fake_proc(self.proc, 11, self.bin, [])
        fake_proc(self.proc, 12, self.bin, [])
        with self.assertRaises(BenchError):
            find_owned_app_pid(10, self.bin, self.proc)


class TestPreviewOracle(unittest.TestCase):
    oracle = {"sha": "a" * 40, "path": "README.md", "content": "x\ny\n", "contentSha256": "0" * 64}

    def test_null_preview_fails(self) -> None:
        for actual in (None, ""):
            with self.assertRaises(BenchError):
                check_source_content(actual, self.oracle)

    def test_different_preview_fails(self) -> None:
        with self.assertRaises(BenchError):
            check_source_content("x\ny", self.oracle)

    def test_exact_preview_passes(self) -> None:
        check_source_content("x\ny\n", self.oracle)

    def test_diff_lines(self) -> None:
        self.assertEqual(missing_diff_lines(None, ["a"]), ["a"])
        self.assertEqual(missing_diff_lines("1 alpha\n2 beta", ["alpha", "gamma"]), ["gamma"])


class TestGitOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.check_output(
            ["git", "-C", self.repo, "-c", "user.name=T", "-c", "user.email=t@x", *args], text=True
        ).strip()

    def write(self, name: str, data: bytes) -> None:
        with open(os.path.join(self.repo, name), "wb") as f:
            f.write(data)

    def test_reads_committed_blob_and_skips_binary(self) -> None:
        self.git("init", "-q", "-b", "main")
        self.write("a.txt", b"one\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "root")
        self.write("0.bin", b"\0\1\2")
        self.write("a.txt", b"one\ntwo\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "second")
        sha = self.git("rev-parse", "HEAD")
        self.write("a.txt", b"one\ntwo\nworking tree only\n")

        o = commit_oracle(self.repo, sha)
        assert o is not None
        self.assertEqual(o["path"], "a.txt")
        self.assertEqual(o["content"], "one\ntwo\n")
        self.assertEqual(o["changedLines"], ["two"])

    def test_matched_ref_uses_dynamic_tip_and_exactly_two_commits(self) -> None:
        self.git("init", "-q", "-b", "feat/divergent")
        self.write("a.txt", b"one\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "root")
        with self.assertRaisesRegex(BenchError, "exactly two"):
            matched_oracle(self.repo)
        self.write("a.txt", b"one\ntwo\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "tip")
        oracle = matched_oracle(self.repo)
        self.assertEqual(oracle["historyOids"], self.git("log", "--topo-order", "--format=%H", MATCHED_REF).splitlines())
        self.assertEqual(oracle["sha"], self.git("rev-parse", "HEAD"))
        self.assertEqual(oracle["path"], "a.txt")
        self.assertEqual(oracle["content"], "one\ntwo\n")
        self.assertIn("+two\n", oracle["patch"])
        self.git("branch", "-m", "different")
        with self.assertRaises(subprocess.CalledProcessError):
            matched_oracle(self.repo)

    def test_merge_commit_is_skipped(self) -> None:
        self.git("init", "-q", "-b", "main")
        self.write("a.txt", b"a\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "root")
        self.git("checkout", "-qb", "side")
        self.write("b.txt", b"b\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "side")
        self.git("checkout", "-q", "main")
        self.write("c.txt", b"c\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "main")
        self.git("merge", "-q", "--no-ff", "-m", "merge", "side")
        self.assertIsNone(commit_oracle(self.repo, self.git("rev-parse", "HEAD")))


class TestMatchedContracts(unittest.TestCase):
    def test_observed_locale_uses_the_language_toggle_not_repo_chooser(self) -> None:
        oracle = {"sha": "a" * 40, "path": "a.txt", "historyOids": ["a" * 40, "b" * 40]}
        d = mock.MagicMock()
        for label, locale in (("中文", "en"), ("English", "zh-Hant"), ("Choose repo / folder", None)):
            d.js.return_value = {**oracle, "ref": MATCHED_REF, "width": 1080, "height": 720,
                                 "previewMode": "diff", "basketEmpty": True, "devicePixelRatio": 1,
                                 "graphPresent": True, "search": "", "languageButton": label}
            observed = matched_tauri_state(d, MATCHED_SENTINEL)
            self.assertEqual(observed["observedLocale"], locale)
            if locale == "en":
                check_matched_state(observed, oracle)
            else:
                with self.assertRaisesRegex(BenchError, "observedLocale"):
                    check_matched_state(observed, oracle)

    def test_oid_order_path_count_geometry_clipboard_and_basket_fail_closed(self) -> None:
        oracle = {"sha": "a" * 40, "path": "a.txt", "historyOids": ["a" * 40, "b" * 40]}
        observed = {**oracle, "ref": MATCHED_REF, "width": 1080, "height": 720,
                    "observedLocale": "en",
                    "previewMode": "diff", "basketEmpty": True,
                    "clipboardSha256": hashlib.sha256(MATCHED_SENTINEL).hexdigest()}
        check_matched_state(observed, oracle)
        for field, invalid in (("sha", "b" * 40), ("path", "other.txt"),
                               ("historyOids", list(reversed(oracle["historyOids"]))),
                               ("historyOids", oracle["historyOids"][:1]), ("width", 1000),
                               ("height", 700), ("ref", "HEAD"), ("previewMode", "content"), ("observedLocale", "zh-Hant"),
                               ("basketEmpty", False), ("clipboardSha256", "changed")):
            with self.subTest(field=field, invalid=invalid), self.assertRaisesRegex(BenchError, field):
                check_matched_state({**observed, field: invalid}, oracle)

    def test_missing_pss_or_short_steady_is_not_a_completed_match(self) -> None:
        measurement = {"status": "COMPLETED", "timestamps": {"steadyDurationSec": 30},
                       "steadyMetrics": {f"{kind}{stat}Mib": 1 for kind in ("rss", "pss") for stat in ("Median", "P95", "Max")}}
        check_matched_measurement(measurement)
        for invalid in (None, 0, float("nan"), float("inf")):
            with self.subTest(value=invalid), self.assertRaisesRegex(BenchError, "pssMedianMib"):
                check_matched_measurement({**measurement, "steadyMetrics": {**measurement["steadyMetrics"], "pssMedianMib": invalid}})
        with self.assertRaisesRegex(BenchError, "30-second"):
            check_matched_measurement({**measurement, "timestamps": {"steadyDurationSec": 29.9}})

    def test_matched_flow_never_visits_content_or_copy(self) -> None:
        oracle = {"sha": "a" * 40, "path": "a.txt", "content": "source", "historyOids": ["a" * 40, "b" * 40],
                  "changedLines": ["added"], "diffRows": [{"kind": "change-addition", "text": "added"}]}
        d = mock.MagicMock()
        d.until.side_effect = lambda what, fn, *args: oracle["historyOids"] if what == "commit rows" else True
        d.js.return_value = oracle["historyOids"]
        with mock.patch("bench_tauri_memory.wait_idle"), mock.patch("bench_tauri_memory.rendered_matched_diff", return_value=oracle["diffRows"]):
            result = bench_tauri_memory.load_repo_preview(d, "/repo", "/shots", matched=oracle)
        clicks = [call.args[0] for call in d.click.call_args_list]
        self.assertIn(f'[data-testid="ref-{MATCHED_REF}"]', clicks)
        self.assertIn(f'[data-commit="{oracle["sha"]}"] span', clicks)
        self.assertFalse(any("content" in css or "copy" in css for css in clicks))
        self.assertFalse(result["sourceContentMatchedGitShow"])

    def test_complete_diff_rejects_missing_extra_reordered_or_changed_rows(self) -> None:
        rows = [{"kind": "change-deletion", "text": "before"}, {"kind": "change-addition", "text": "after"}]
        oracle = {"diffRows": rows}
        check_matched_diff(rows, oracle)
        for bad in (rows[:1], rows + [rows[0]], list(reversed(rows)),
                    [rows[0], {"kind": "change-addition", "text": "wrong"}],
                    [rows[0], {"kind": "change-addition", "text": None}]):
            with self.subTest(rows=bad), self.assertRaisesRegex(BenchError, "diff row"):
                check_matched_diff(bad, oracle)

    def test_legacy_flow_still_verifies_content_then_diff(self) -> None:
        oracle = {"sha": "a" * 40, "path": "a.txt", "content": "source", "changedLines": ["added"]}
        d = mock.MagicMock()
        d.until.side_effect = lambda what, fn, *args: [oracle["sha"]] if what == "commit rows" else True
        with mock.patch("bench_tauri_memory.wait_idle"), mock.patch("bench_tauri_memory.commit_oracle", return_value=oracle):
            result = bench_tauri_memory.load_repo_preview(d, "/repo", "/shots")
        clicks = [call.args[0] for call in d.click.call_args_list]
        self.assertIn('[data-testid="preview-content"]', clicks)
        self.assertLess(clicks.index('[data-testid="preview-content"]'), clicks.index('[data-testid="preview-diff"]'))
        self.assertTrue(result["sourceContentMatchedGitShow"])

    def test_identity_detects_worktree_and_binary_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            repo = os.path.join(root, "repo")
            os.mkdir(repo)
            subprocess.run(["git", "init", "-q", repo], check=True)
            path = os.path.join(repo, "file.txt")
            with open(path, "w") as f:
                f.write("before")
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(["git", "-C", repo, "-c", "user.name=T", "-c", "user.email=t@x", "commit", "-qm", "init"], check=True)
            manifest = os.path.join(root, "workload_manifest.json")
            with open(manifest, "w") as f:
                json.dump({"preset": "standard", "repos": [{"name": "repo"}]}, f)
            binary = os.path.join(root, "binary")
            with open(binary, "w") as f:
                f.write("binary")
            before = matched_identity(binary, repo)
            self.assertEqual(before, matched_identity(binary, repo))
            with open(path, "w") as f:
                f.write("after")
            self.assertNotEqual(before["repoSha256"], matched_identity(binary, repo)["repoSha256"])
            with open(binary, "a") as f:
                f.write("changed")
            self.assertNotEqual(before["binarySha256"], matched_identity(binary, repo)["binarySha256"])


class TestAttachGuards(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.target = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])

    def tearDown(self) -> None:
        if self.target.poll() is None:
            self.target.kill()
        self.target.wait()
        self.tmp.cleanup()

    def ready_later(self, path: str, content: str, delay: float = 0.1) -> threading.Thread:
        def write() -> None:
            time.sleep(delay)
            with open(path, "w") as f:
                f.write(content)

        t = threading.Thread(target=write, daemon=True)
        t.start()
        return t

    def test_reused_pid_fails(self) -> None:
        start = read_proc_starttime(self.target.pid)
        assert start is not None
        with self.assertRaises(ProcessCrashedError):
            measure_single_profile(
                attach_pid=self.target.pid, attach_starttime=start + 1,
                ready_marker="[READY:X]", steady_seconds=0.2, sample_interval=0.02, readiness_timeout=1.0,
            )

    def test_attached_process_dying_mid_steady_fails(self) -> None:
        ready = os.path.join(self.tmp.name, "ready")
        self.ready_later(ready, "[READY:X]\n")
        threading.Timer(0.5, self.target.kill).start()
        with self.assertRaises(ProcessCrashedError):
            measure_single_profile(
                attach_pid=self.target.pid, ready_file=ready, ready_marker="[READY:X]",
                steady_seconds=5.0, sample_interval=0.02, readiness_timeout=2.0,
            )

    def test_attach_never_signals_target(self) -> None:
        ready = os.path.join(self.tmp.name, "ready")
        self.ready_later(ready, "[READY:X]\n")
        result = measure_single_profile(
            attach_pid=self.target.pid, ready_file=ready, ready_marker="[READY:X]",
            steady_seconds=0.2, sample_interval=0.02, readiness_timeout=2.0,
        )
        self.assertIsNone(self.target.poll())
        self.assertEqual(result["mode"], "attach")
        self.assertIsNone(result["timestamps"]["launchDurationSec"])
        self.assertEqual({s["phase"] for s in result["samples"]} - {"steady"}, {"pre-ready"})

    def test_stale_ready_file_fails(self) -> None:
        ready = os.path.join(self.tmp.name, "ready")
        with open(ready, "w") as f:
            f.write("[READY:X]\n")
        with self.assertRaises(HarnessError):
            measure_single_profile(
                attach_pid=self.target.pid, ready_file=ready, ready_marker="[READY:X]",
                steady_seconds=0.2, sample_interval=0.02, readiness_timeout=1.0,
            )

    def test_ready_file_read_is_bounded(self) -> None:
        ready = os.path.join(self.tmp.name, "ready")
        self.ready_later(ready, "x" * 1_000_000 + "[READY:X]")
        with self.assertRaises(ReadinessTimeoutError):
            measure_single_profile(
                attach_pid=self.target.pid, ready_file=ready, ready_marker="[READY:X]",
                steady_seconds=0.2, sample_interval=0.02, readiness_timeout=0.6,
            )


def completed_run(run_dir: str, samples: list[tuple[str, float]]) -> dict:
    """A COMPLETED run record shaped like run_profile's output, with raw samples on disk."""
    os.makedirs(run_dir, exist_ok=True)
    with open(os.path.join(run_dir, "raw_samples.jsonl"), "w") as f:
        for phase, mib in samples:
            f.write(json.dumps({"phase": phase, "totalRssMib": mib}) + "\n")
    steady = [m for p, m in samples if p == "steady"]
    return {
        "profile": "idle", "runDir": run_dir, "status": "COMPLETED", "cleanupProblems": [],
        "measurement": {
            "steadyMetrics": {"rssMedianMib": steady[0], "rssMaxMib": max(steady), "pssMedianMib": 1.0},
            "overallPeak": {"sampledPeakRssMib": max(m for _, m in samples)},
            "mainProcessVmHwm": {"mib": 2.0},
            "timestamps": {"attachToReadySec": 0.5},
            "processTree": {"steadyMedianProcessCount": 3},
        },
        "processAgeAtSamplerStartSec": 0.06,
        "app": {"pid": 1}, "ui": {}, "controllerAtEnd": {"rssMib": 1, "pssMib": 1},
        "uiEnvironment": {"innerWidth": 1, "innerHeight": 1, "devicePixelRatio": 1, "visibilityState": "visible", "graphicsLibsMapped": {}},
    }


class FakeDriver:
    stop_problems: list[str] = []

    def __init__(self, bin_path: str, log_path: str):
        self.cmd = ["fake-driver"]
        self.isolation = {}

    def stop(self) -> list[str]:
        return list(self.stop_problems)


def fake_drive(d, profile, repo, run_dir, steady, interval, result) -> None:
    result.update(completed_run(run_dir, [("pre-ready", 10.0), ("steady", 20.0)]))
    del result["status"], result["cleanupProblems"]


class TestCleanupFailsRun(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        patches = [mock.patch.object(bench_tauri_memory, "Driver", FakeDriver),
                   mock.patch.object(bench_tauri_memory, "drive", fake_drive)]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        self.addCleanup(signal.signal, signal.SIGTERM, signal.getsignal(signal.SIGTERM))

    def tearDown(self) -> None:
        FakeDriver.stop_problems = []
        self.tmp.cleanup()

    def run_one(self) -> dict:
        return bench_tauri_memory.run_profile("idle", "/bin/true", "", os.path.join(self.tmp.name, "r"), 1.0, 0.05)

    def test_clean_teardown_completes(self) -> None:
        self.assertEqual(self.run_one()["status"], "COMPLETED")

    def test_straggler_fails_run_and_keeps_measurement(self) -> None:
        FakeDriver.stop_problems = ["owned WebKitWebProces pid 7 survived teardown for 5.0s; sent SIGKILL"]
        r = self.run_one()
        self.assertEqual(r["status"], "FAILED")
        self.assertIn("survived teardown", r["error"])
        self.assertEqual(r["measurement"]["steadyMetrics"]["rssMedianMib"], 20.0)
        self.assertEqual(r["cleanupProblems"], FakeDriver.stop_problems)

    def test_drive_error_keeps_partial_diagnostics(self) -> None:
        def partial(d, profile, repo, run_dir, steady, interval, result):
            result["app"] = {"pid": 42}
            raise BenchError("diff pane misses ['x']")

        with mock.patch.object(bench_tauri_memory, "drive", partial):
            r = self.run_one()
        self.assertEqual(r["status"], "FAILED")
        self.assertEqual(r["app"], {"pid": 42})
        self.assertIn("diff pane misses", r["error"])

    def test_cleanup_problem_fails_command_and_summary(self) -> None:
        FakeDriver.stop_problems = ["driver process group: still alive"]
        ws = os.path.join(self.tmp.name, "ws")
        os.makedirs(ws)
        with open(os.path.join(ws, "workload_manifest.json"), "w") as f:
            json.dump({"workloadRevision": "t", "seed": 1, "preset": "smoke", "parameters": {},
                       "summary": {"totalRepos": 1, "totalCommits": 1}}, f)
        out = os.path.join(self.tmp.name, "out")
        code = bench_tauri_memory.main(["--bin", sys.executable, "--workspace", ws, "--out-dir", out, "--profile", "idle"])
        self.assertEqual(code, 1)
        with open(os.path.join(out, "tauri_baseline_report.json")) as f:
            prof = json.load(f)["profiles"]["idle"]
        self.assertEqual(prof["summary"], {"completedRuns": 0, "failedRuns": 1})
        self.assertEqual(prof["runs"][0]["cleanupProblems"], FakeDriver.stop_problems)
        self.assertIn("measurement", prof["runs"][0])


class TestReapOwned(unittest.TestCase):
    def spawn(self) -> subprocess.Popen:
        # Ignores SIGTERM, like a helper that survives polite group cleanup.
        p = subprocess.Popen([sys.executable, "-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)"])
        self.addCleanup(lambda: (p.poll() is None and p.kill(), p.wait()))
        time.sleep(0.1)
        return p

    def test_survivor_is_killed_and_reported_unrelated_untouched(self) -> None:
        owned, unrelated = self.spawn(), self.spawn()
        reaper = threading.Thread(target=owned.wait)  # reap the zombie like its real parent would
        reaper.start()
        problems = reap_owned([identity(owned.pid)], grace=0.2)
        reaper.join(timeout=5)
        self.assertEqual(len(problems), 1)
        self.assertIn("survived teardown", problems[0])
        self.assertIsNotNone(owned.returncode)
        self.assertIsNone(unrelated.poll())

    def test_gone_processes_are_clean(self) -> None:
        p = subprocess.Popen([sys.executable, "-c", "pass"])
        ident = identity(p.pid)
        p.wait()
        self.assertEqual(reap_owned([ident], grace=0.2), [])


class TestPhasePeaksAndRegenerate(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_peaks_are_phase_scoped(self) -> None:
        run = completed_run(os.path.join(self.tmp.name, "r1"), [("pre-ready", 30.0), ("pre-ready", 40.0), ("steady", 50.0), ("steady", 45.0)])
        s = summarize([run])
        self.assertEqual(s["preReadySampledPeakRssMib"]["worst"], 40.0)
        self.assertEqual(s["steadySampledPeakRssMib"]["worst"], 50.0)
        self.assertEqual(s["attachToEndSampledPeakRssMib"]["worst"], 50.0)

    def test_regenerate_keeps_original_and_states_provenance(self) -> None:
        run = completed_run(os.path.join(self.tmp.name, "idle", "run-01"), [("pre-ready", 30.0), ("steady", 20.0)])
        original = {"timestampUtc": "t", "runsPerProfile": 1, "steadySeconds": 1, "sampleIntervalSec": 0.05,
                    "metadata": {"gitHead": "h", "gitDirtyPaths": [], "binary": {"path": "b", "sha256": "s", "buildCommand": "c", "buildProfile": "p"},
                                 "webkit2gtk": "w", "system": {"os": "o", "kernel": "k", "cpu": "c"},
                                 "dataset": {"path": "d", "workloadRevision": "r", "seed": 1, "summary": {"totalRepos": 15, "totalCommits": 1}}},
                    "profiles": {"idle": {"summary": {}, "runs": [run]}}}
        src = os.path.join(self.tmp.name, "tauri_baseline_report.json")
        with open(src, "w") as f:
            json.dump(original, f)
        with open(src) as f:
            before = f.read()
        self.assertEqual(bench_tauri_memory.main(["--regenerate", self.tmp.name]), 0)
        with open(src) as f:
            self.assertEqual(f.read(), before)
        with open(os.path.join(self.tmp.name, "tauri_baseline_report.regenerated.json")) as f:
            regen = json.load(f)
        self.assertEqual(regen["profiles"]["15repo"]["status"], "UNSUPPORTED")
        self.assertIn("predates 15repo serialization", regen["provenance"])
        self.assertEqual(regen["profiles"]["idle"]["summary"]["preReadySampledPeakRssMib"]["worst"], 30.0)


if __name__ == "__main__":
    unittest.main()
