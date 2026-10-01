#!/usr/bin/env python3
"""
test_harness_contracts.py - Focused Contract Tests for Linux Memory Benchmark Harness.

Validates:
1. Literal marker match (not regex: single characters in [READY:IDLE] do not trigger match).
2. Explicit argv array preserves arguments with spaces without shell mangling.
3. Process group cleanup terminates child processes (no orphan processes leaked).
4. External SIGTERM to harness process triggers signal handler and reaps child process.
5. Missing readiness marker strictly fails (raises ReadinessTimeoutError, no fake zeros).
6. Premature process crash strictly fails (raises ProcessCrashedError with exit code).
7. Invalid timing parameters (NaN, <=0) and empty marker fail before spawn.
8. Unreadable root / zero RSS fails strictly (no successful zeros emitted).
9. Missing/unreadable child PID is explicitly recorded in unreadableChildrenPids.
10. Honest counter provenance and separate main VmHWM.
11. Bounded output monitor handles multi-megabyte output and splits markers without memory blowup.
"""

from __future__ import annotations

import math
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


def require_or_skip(cond: bool, reason: str) -> None:
    if cond:
        return
    assert os.environ.get("SNIP_REQUIRE_ALL_TESTS") is None, reason
    raise unittest.SkipTest(reason)

# Add scripts directory to sys.path
SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from memory_harness import (
    BinaryNotFoundError,
    BoundedOutputMonitor,
    HarnessError,
    ProcessCrashedError,
    ProcessTreeSampler,
    ReadinessTimeoutError,
    ZeroSamplesError,
    measure_single_profile,
    read_process_identity,
)


class TestHarnessContracts(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_literal_marker_detection_not_regex(self) -> None:
        """
        Verify literal marker matching:
        Emitting single characters from '[READY:IDLE]' must NOT trigger readiness.
        A regex like 'grep [READY:IDLE]' would match any of 'R', 'E', 'A', 'D', ':', etc.
        Readiness must only trigger when the literal '[READY:IDLE]' string appears.
        """
        fixture_code = """
import sys, time
# Emit individual characters from [READY:IDLE]
for ch in "READY:IDLE[]":
    sys.stdout.write(ch + "\\n")
    sys.stdout.flush()
    time.sleep(0.02)

# Sleep to ensure harness does not trigger prematurely on single chars
time.sleep(0.15)

# Now emit the literal marker
sys.stdout.write("[READY:IDLE]\\n")
sys.stdout.flush()

# Hold alive for steady measurement
time.sleep(1.0)
"""
        argv = [sys.executable, "-c", fixture_code]
        result = measure_single_profile(
            argv=argv,
            ready_marker="[READY:IDLE]",
            profile_label="Test Literal Marker",
            steady_seconds=0.3,
            sample_interval=0.04,
            readiness_timeout=2.0,
        )

        self.assertEqual(result["status"], "COMPLETED")
        self.assertIsNotNone(result["timestamps"]["readyMonotonic"])
        self.assertGreaterEqual(result["timestamps"]["launchDurationSec"], 0.25)

    def test_spaces_in_argv(self) -> None:
        """
        Verify that arguments containing spaces are passed intact to the subprocess
        without being split, unquoted, or mangled.
        """
        arg_with_spaces_1 = "--workspace-path"
        arg_with_spaces_2 = "/tmp/snip sync path with spaces/sub dir"
        arg_with_spaces_3 = "special \"quoted\" value with spaces"

        fixture_code = """
import sys, time
args = sys.argv[1:]
expected_2 = "/tmp/snip sync path with spaces/sub dir"
expected_3 = "special \\\"quoted\\\" value with spaces"

if args[1] != expected_2:
    sys.stderr.write(f"Arg 1 mismatch: {args[1]!r} != {expected_2!r}\\n")
    sys.exit(10)

if args[2] != expected_3:
    sys.stderr.write(f"Arg 2 mismatch: {args[2]!r} != {expected_3!r}\\n")
    sys.exit(11)

sys.stdout.write("[READY:SPACES_OK]\\n")
sys.stdout.flush()
time.sleep(1.0)
"""
        argv = [sys.executable, "-c", fixture_code, arg_with_spaces_1, arg_with_spaces_2, arg_with_spaces_3]
        result = measure_single_profile(
            argv=argv,
            ready_marker="[READY:SPACES_OK]",
            profile_label="Test Spaces Argv",
            steady_seconds=0.2,
            sample_interval=0.04,
            readiness_timeout=2.0,
        )
        self.assertEqual(result["status"], "COMPLETED")
        self.assertEqual(result["argv"], argv)

    def test_child_process_group_cleanup(self) -> None:
        """
        Verify that process group cleanup properly terminates child processes
        spawned by the target process, leaving no orphan processes behind.
        """
        child_pid_file = os.path.join(self.temp_dir.name, "child.pid")

        fixture_code = f"""
import subprocess, sys, time, os

# Spawn a background child process that sleeps
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
with open("{child_pid_file}", "w") as f:
    f.write(str(child.pid))

sys.stdout.write("[READY:CHILD_SPAWNED]\\n")
sys.stdout.flush()
time.sleep(2.0)
"""
        argv = [sys.executable, "-c", fixture_code]
        result = measure_single_profile(
            argv=argv,
            ready_marker="[READY:CHILD_SPAWNED]",
            profile_label="Test Child Cleanup",
            steady_seconds=0.2,
            sample_interval=0.04,
            readiness_timeout=2.0,
        )
        self.assertEqual(result["status"], "COMPLETED")

        self.assertTrue(os.path.exists(child_pid_file))
        with open(child_pid_file) as f:
            child_pid = int(f.read().strip())

        time.sleep(0.1)
        child_alive = True
        try:
            os.kill(child_pid, 0)
        except (ProcessLookupError, OSError):
            child_alive = False

        self.assertFalse(child_alive, f"Child process {child_pid} was leaked by harness cleanup!")

    def test_external_sigterm_to_harness_cleans_child(self) -> None:
        """
        Verify that when the harness receives an external SIGTERM,
        its signal handler cleans up the process group so child processes do not orphan.
        """
        child_pid_file = os.path.join(self.temp_dir.name, "sigterm_child.pid")
        out_dir = os.path.join(self.temp_dir.name, "sigterm_out")

        # Command run by harness: launches a long-lived child
        child_cmd = f"""
import subprocess, sys, time
p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
with open('{child_pid_file}', 'w') as f:
    f.write(str(p.pid))
sys.stdout.write('[READY:CHILD]\\n')
sys.stdout.flush()
time.sleep(30)
"""
        # Launch harness as external subprocess
        harness_argv = [
            sys.executable,
            os.path.join(SCRIPTS_DIR, "memory_harness.py"),
            "--ready-marker", "[READY:CHILD]",
            "--profile-label", "SIGTERM Test",
            "--steady-seconds", "30.0",
            "--out-dir", out_dir,
            "--",
            sys.executable, "-c", child_cmd,
        ]

        harness_proc = subprocess.Popen(harness_argv)

        # Wait until child PID file is populated
        child_pid = None
        for _ in range(50):
            if os.path.exists(child_pid_file):
                try:
                    with open(child_pid_file) as f:
                        content = f.read().strip()
                        if content:
                            child_pid = int(content)
                            break
                except Exception:
                    pass
            time.sleep(0.05)

        self.assertIsNotNone(child_pid, "Target child did not start or write PID in time.")

        # Verify child is alive
        os.kill(child_pid, 0)

        # Send SIGTERM to the harness process
        harness_proc.terminate()
        harness_proc.wait(timeout=2.0)

        # Verify that child process was terminated by the signal cleanup handler
        time.sleep(0.15)
        child_alive = True
        try:
            os.kill(child_pid, 0)
        except (ProcessLookupError, OSError):
            child_alive = False

        self.assertFalse(child_alive, f"Child PID {child_pid} survived external SIGTERM to harness!")

    def test_missing_readiness_marker_fails(self) -> None:
        """Verify that missing readiness marker causes ReadinessTimeoutError."""
        fixture_code = """
import sys, time
sys.stdout.write("Working hard without ready marker...\\n")
sys.stdout.flush()
time.sleep(5.0)
"""
        argv = [sys.executable, "-c", fixture_code]
        with self.assertRaises(ReadinessTimeoutError):
            measure_single_profile(
                argv=argv,
                ready_marker="[READY:WILL_NEVER_ARRIVE]",
                profile_label="Test Missing Marker",
                steady_seconds=1.0,
                sample_interval=0.04,
                readiness_timeout=0.4,
            )

    def test_process_crash_fails(self) -> None:
        """Verify that premature process crash raises ProcessCrashedError with exit code."""
        fixture_code = """
import sys, time
sys.stdout.write("Crashing now...\\n")
sys.stdout.flush()
sys.exit(42)
"""
        argv = [sys.executable, "-c", fixture_code]
        with self.assertRaises(ProcessCrashedError) as ctx:
            measure_single_profile(
                argv=argv,
                ready_marker="[READY:IDLE]",
                profile_label="Test Crash",
                steady_seconds=1.0,
                readiness_timeout=2.0,
            )
        self.assertIn("42", str(ctx.exception))

    def test_process_exit_during_a_sample_keeps_exit_code(self) -> None:
        """A child that exits between poll() and a sample is still a crash with its exit code.

        The race failed test_process_crash_fails under load ("exited or was reused across
        a sample", no 42); here every sample waits until the child is gone."""
        require_or_skip(sys.platform.startswith("linux"), "the sampler reads /proc")
        real_sample_tree = ProcessTreeSampler.sample_tree

        def sample_then_wait_for_exit(sampler: ProcessTreeSampler) -> dict:
            instant = real_sample_tree(sampler)
            deadline = time.monotonic() + 10.0
            while read_process_identity(sampler.root_pid, sampler.proc_root) is not None:
                self.assertLess(time.monotonic(), deadline, "fixture never exited")
                time.sleep(0.01)
            return instant

        fixture_code = "import sys, time\ntime.sleep(0.3)\nsys.exit(42)\n"
        with patch.object(ProcessTreeSampler, "sample_tree", sample_then_wait_for_exit):
            with self.assertRaises(ProcessCrashedError) as ctx:
                measure_single_profile(
                    argv=[sys.executable, "-c", fixture_code],
                    ready_marker="[READY:IDLE]",
                    profile_label="Test Crash Mid-Sample",
                    steady_seconds=1.0,
                    readiness_timeout=5.0,
                )
        self.assertIn("exit code 42", str(ctx.exception))

    def test_missing_binary_fails(self) -> None:
        """Verify that a non-existent binary raises BinaryNotFoundError."""
        with self.assertRaises(BinaryNotFoundError):
            measure_single_profile(
                argv=["/nonexistent/path/to/binary_12345"],
                ready_marker="[READY:IDLE]",
                profile_label="Test Missing Binary",
            )

    def test_invalid_parameters_before_spawn_fails(self) -> None:
        """Verify that NaN, non-positive numbers, and empty marker fail with ValueError before spawn."""
        valid_argv = [sys.executable, "-c", "import sys; sys.exit(0)"]

        # 1. Empty readiness marker
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="", profile_label="T")
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="   ", profile_label="T")

        # 2. NaN or <= 0 steady_seconds
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", steady_seconds=float("nan"))
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", steady_seconds=0.0)

        # 3. NaN or <= 0 sample_interval
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", sample_interval=float("nan"))
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", sample_interval=-0.1)

        # 4. NaN or <= 0 readiness_timeout
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", readiness_timeout=float("nan"))
        with self.assertRaises(ValueError):
            measure_single_profile(valid_argv, ready_marker="[R]", profile_label="T", readiness_timeout=0.0)

    def test_unreadable_root_zero_rss_fails(self) -> None:
        """
        Verify that if root process /proc is unreadable or returns zero RSS,
        ProcessTreeSampler marks sample invalid, and the harness strictly fails
        rather than emitting successful zeros.
        """
        fake_proc_dir = os.path.join(self.temp_dir.name, "fake_proc")
        os.makedirs(os.path.join(fake_proc_dir, "99999", "task"), exist_ok=True)

        sampler = ProcessTreeSampler(root_pid=99999, pgrp=99999, proc_root=fake_proc_dir)
        sample = sampler.sample_tree()

        self.assertFalse(sample["valid"])
        self.assertFalse(sample["rootSampled"])
        self.assertEqual(sample["totalRssBytes"], 0)

    def test_missing_child_recorded_explicitly(self) -> None:
        """
        Verify that when a child process PID exists in tree but becomes unreadable,
        it is explicitly listed in unreadableChildrenPids and not treated as zero bytes.
        """
        fake_proc_dir = os.path.join(self.temp_dir.name, "fake_proc_tree")
        root_dir = os.path.join(fake_proc_dir, "100", "task", "100")
        os.makedirs(root_dir, exist_ok=True)

        # Mock root smaps_rollup with 1024 kB RSS
        with open(os.path.join(fake_proc_dir, "100", "smaps_rollup"), "w") as f:
            f.write("Rss:                1024 kB\nPss:                 512 kB\n")

        # Mock child 101 listed in children but with unreadable/missing /proc/101
        with open(os.path.join(root_dir, "children"), "w") as f:
            f.write("101\n")

        sampler = ProcessTreeSampler(root_pid=100, pgrp=100, proc_root=fake_proc_dir)
        sample = sampler.sample_tree()

        self.assertTrue(sample["valid"])
        self.assertTrue(sample["rootSampled"])
        self.assertEqual(sample["rootRssBytes"], 1024 * 1024)
        self.assertIn(101, sample["unreadableChildrenPids"])
        self.assertEqual(sample["pids"], [100])

    def test_bounded_output_monitor_huge_line(self) -> None:
        """
        Verify BoundedOutputMonitor handles large output chunks without RAM explosion
        and accurately detects literal markers even across chunk splits.
        """
        monitor = BoundedOutputMonitor(marker="[READY:SPLIT_MARKER]", max_log_bytes=4096)

        # Feed 100 KB of filler text
        huge_chunk = "A" * 10000
        for _ in range(10):
            monitor.add_chunk(huge_chunk)

        self.assertFalse(monitor.is_marker_found())
        # Log tail must remain bounded to max_log_bytes
        self.assertLessEqual(len(monitor.get_diagnostic_log().encode("utf-8")), 8192)

        # Feed split marker across two chunks: "[READY:" then "SPLIT_MARKER]"
        monitor.add_chunk("prefix_padding[READY:")
        self.assertFalse(monitor.is_marker_found())
        monitor.add_chunk("SPLIT_MARKER]suffix_padding")
        self.assertTrue(monitor.is_marker_found())

    def test_honest_counter_provenance_and_main_vmhwm(self) -> None:
        """
        Verify that RSS and PSS have honest counter provenance,
        main process VmHWM is recorded separately, and statistics are computed.
        """
        fixture_code = """
import sys, time
# Allocate memory buffer
buf = bytearray(15 * 1024 * 1024)
for i in range(0, len(buf), 4096):
    buf[i] = 1

sys.stdout.write("[READY:MEM_READY]\\n")
sys.stdout.flush()
time.sleep(1.0)
"""
        argv = [sys.executable, "-c", fixture_code]
        result = measure_single_profile(
            argv=argv,
            ready_marker="[READY:MEM_READY]",
            profile_label="Test Memory Provenance",
            steady_seconds=0.3,
            sample_interval=0.04,
            readiness_timeout=2.0,
        )

        steady = result["steadyMetrics"]
        self.assertGreaterEqual(steady["rssMedianMib"], 10.0)
        self.assertGreaterEqual(steady["rssMaxMib"], steady["rssMedianMib"])

        hwm = result["mainProcessVmHwm"]
        self.assertIsNotNone(hwm["bytes"])
        self.assertGreaterEqual(hwm["mib"], 10.0)
        self.assertEqual(hwm["provenance"], "status_vmhwm")

        prov = result["counterProvenance"]
        self.assertIn("smaps_rollup", prov["rss"])
        self.assertIn("status_vmhwm", prov["hwm"])

        intervals = result["sampleInterval"]
        self.assertGreater(intervals["actualSampleCount"], 5)
        self.assertGreater(intervals["actualMedianSec"], 0.0)

    def test_attach_pid_and_ready_file_seam(self) -> None:
        """
        Verify that attach_pid attaches to an existing running PID and
        ready_file detects the readiness marker written to a file.
        """
        import subprocess
        import threading

        # Start a dummy process that sleeps for 5 seconds
        target_proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        ready_path = os.path.join(self.temp_dir.name, "ready_signal.txt")

        # Writer thread writes ready marker after 0.1s
        def write_ready() -> None:
            time.sleep(0.1)
            with open(ready_path, "w", encoding="utf-8") as f:
                f.write("[READY:ATTACH_SEAM]\n")

        t = threading.Thread(target=write_ready, daemon=True)
        t.start()

        try:
            result = measure_single_profile(
                attach_pid=target_proc.pid,
                ready_file=ready_path,
                ready_marker="[READY:ATTACH_SEAM]",
                profile_label="Test Attach & Ready File",
                steady_seconds=0.3,
                sample_interval=0.04,
                readiness_timeout=2.0,
            )
            self.assertEqual(result["processTree"]["rootPid"], target_proc.pid)
            self.assertGreater(result["steadyMetrics"]["rssMedianMib"], 0.0)
            self.assertEqual(result["argv"], [f"(attached-pid:{target_proc.pid})"])
        finally:
            t.join(timeout=0.5)
            target_proc.terminate()
            target_proc.wait(timeout=1.0)

    def test_attach_pid_missing_fails(self) -> None:
        """Verify that attaching to a non-existent PID raises BinaryNotFoundError."""
        with self.assertRaises(BinaryNotFoundError):
            measure_single_profile(
                attach_pid=99999999,
                ready_marker="[READY:IDLE]",
                profile_label="Test Nonexistent Attached PID",
            )

    def test_ready_file_timeout_fails(self) -> None:
        """Verify that if ready_file never receives the marker, ReadinessTimeoutError is raised."""
        ready_path = os.path.join(self.temp_dir.name, "never_ready.txt")
        fixture_code = "import time; time.sleep(5)"
        argv = [sys.executable, "-c", fixture_code]
        with self.assertRaises(ReadinessTimeoutError):
            measure_single_profile(
                argv=argv,
                ready_file=ready_path,
                ready_marker="[READY:UNSEEN]",
                profile_label="Test Ready File Timeout",
                steady_seconds=0.5,
                sample_interval=0.04,
                readiness_timeout=0.3,
            )

    def test_sampler_ready_file_preexisting_rejected(self) -> None:
        """Verify that if sampler_ready_file exists beforehand, HarnessError is raised."""
        stale_ready = os.path.join(self.temp_dir.name, "stale_ready.signal")
        with open(stale_ready, "w") as f:
            f.write("STALE")

        with self.assertRaises(HarnessError) as cm:
            measure_single_profile(
                argv=[sys.executable, "-c", "import time; time.sleep(1)"],
                ready_marker="[READY]",
                sampler_ready_file=stale_ready,
                profile_label="Test Stale Ready",
            )
        self.assertIn("already exists before the run", str(cm.exception))

    def test_sampler_gate_before_same_image_is_not_launch(self) -> None:
        """A gate unblocks work in the process we already attached. That is not an exec transition."""
        sampler_ready_path = os.path.join(self.temp_dir.name, "sampler_ready.signal")
        ready_path = os.path.join(self.temp_dir.name, "app_ready.signal")
        child_code = (
            "import os, sys, time\n"
            "ready_sig, app_ready, marker = sys.argv[1], sys.argv[2], sys.argv[3]\n"
            "for _ in range(1000):\n"
            "    if os.path.exists(ready_sig):\n"
            "        break\n"
            "    time.sleep(0.005)\n"
            "else:\n"
            "    sys.exit(1)\n"
            "chunk = bytearray(20 * 1024 * 1024)\n"
            "for i in range(0, len(chunk), 4096):\n"
            "    chunk[i] = 1\n"
            "time.sleep(0.12)\n"
            "with open(app_ready, 'w') as f:\n"
            "    f.write(marker + '\\n')\n"
            "time.sleep(0.4)\n"
        )
        child = subprocess.Popen(
            [sys.executable, "-c", child_code, sampler_ready_path, ready_path, "[READY:GATE]"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        previous = signal.getsignal(signal.SIGINT)
        try:
            result = measure_single_profile(
                attach_pid=child.pid,
                expected_exe=sys.executable,
                ready_file=ready_path,
                ready_marker="[READY:GATE]",
                sampler_ready_file=sampler_ready_path,
                profile_label="gate without exec",
                steady_seconds=0.25,
                sample_interval=0.03,
                readiness_timeout=3.0,
            )
            self.assertTrue(os.path.exists(sampler_ready_path))
            self.assertFalse(result["samplerGate"]["preExec"])
            self.assertIsNone(result["timestamps"]["execDiscoveredMonotonic"])
            self.assertIsNone(result["launchMetrics"])
            self.assertIsNone(result["launchClaim"])
            self.assertEqual(result["sampleInterval"]["launchSampleCount"], 0)
            self.assertGreater(result["sampleInterval"]["preReadySampleCount"], 0)
            peak = max(s["totalRssMib"] for s in result["samples"] if s["phase"] in ("pre-ready", "steady"))
            self.assertGreaterEqual(peak, 18.0)
        finally:
            child.kill()
            child.wait(timeout=2)
        self.assertEqual(signal.getsignal(signal.SIGINT), previous)

    def test_ready_marker_on_unexpected_exe_fails_closed(self) -> None:
        """The supervisor probe: a sleeping Python process is not /bin/sleep, even if the ready file says so."""
        ready_path = os.path.join(self.temp_dir.name, "ready.signal")
        gate = os.path.join(self.temp_dir.name, "gate.signal")
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(4)"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )

        def write_ready() -> None:
            time.sleep(0.15)
            with open(ready_path, "w", encoding="utf-8") as f:
                f.write("READY-PROBE\n")

        writer = threading.Thread(target=write_ready, daemon=True)
        writer.start()
        previous = signal.getsignal(signal.SIGINT)
        try:
            with self.assertRaises(HarnessError) as cm:
                measure_single_profile(
                    attach_pid=child.pid,
                    expected_exe="/bin/sleep",
                    sampler_ready_file=gate,
                    ready_file=ready_path,
                    ready_marker="READY-PROBE",
                    profile_label="wrong exe",
                    steady_seconds=0.2,
                    sample_interval=0.02,
                    readiness_timeout=3.0,
                )
            self.assertIn("does not certify", str(cm.exception))
            self.assertIsNone(child.poll())
        finally:
            writer.join(timeout=2)
            child.kill()
            child.wait(timeout=2)
        self.assertEqual(signal.getsignal(signal.SIGINT), previous)

    def test_late_attach_matching_exe_is_pre_ready(self) -> None:
        require_or_skip(os.path.isfile("/bin/sleep") and os.access("/bin/sleep", os.X_OK), "/bin/sleep missing")
        ready_path = os.path.join(self.temp_dir.name, "ready.signal")
        gate = os.path.join(self.temp_dir.name, "gate.signal")
        child = subprocess.Popen(["/bin/sleep", "5"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        def write_ready() -> None:
            time.sleep(0.15)
            with open(ready_path, "w", encoding="utf-8") as f:
                f.write("READY-PROBE\n")

        writer = threading.Thread(target=write_ready, daemon=True)
        writer.start()
        sleep_exe = os.path.realpath("/bin/sleep")
        try:
            result = measure_single_profile(
                attach_pid=child.pid,
                expected_exe="/bin/sleep",
                sampler_ready_file=gate,
                ready_file=ready_path,
                ready_marker="READY-PROBE",
                profile_label="late attach",
                steady_seconds=0.2,
                sample_interval=0.02,
                readiness_timeout=3.0,
            )
            self.assertIsNone(result["timestamps"]["execDiscoveredMonotonic"])
            self.assertIsNone(result["launchMetrics"])
            self.assertIsNone(result["launchClaim"])
            self.assertEqual(result["sampleInterval"]["launchSampleCount"], 0)
            self.assertGreater(result["sampleInterval"]["preReadySampleCount"], 0)
            self.assertGreaterEqual(result["sampleInterval"]["steadySampleCount"], 8)
            self.assertFalse(result["samplerGate"]["preExec"])
            self.assertEqual({s["phase"] for s in result["samples"]}, {"pre-ready", "steady"})
            self.assertTrue(all(s["exe"] == sleep_exe for s in result["samples"]))
            self.assertIsNone(child.poll())
        finally:
            writer.join(timeout=2)
            child.kill()
            child.wait(timeout=2)

    def test_exec_transition_excludes_helper_rss(self) -> None:
        """Python touches ~60 MiB, then execs a small shell. Target peaks must not include that helper."""
        sh_bin = shutil.which("sh")
        require_or_skip(bool(sh_bin), "sh is missing")
        sh_bin = os.path.realpath(sh_bin)
        py_exe = os.path.realpath(sys.executable)
        sampler_ready_path = os.path.join(self.temp_dir.name, "sampler_ready.signal")
        ready_path = os.path.join(self.temp_dir.name, "app.ready")
        launcher_code = (
            "import os, sys, time\n"
            "ready_sig, app_ready, sh_target = sys.argv[1], sys.argv[2], sys.argv[3]\n"
            "chunk = bytearray(60 * 1024 * 1024)\n"
            "for i in range(0, len(chunk), 4096):\n"
            "    chunk[i] = 1\n"
            "for _ in range(1000):\n"
            "    if os.path.exists(ready_sig):\n"
            "        break\n"
            "    time.sleep(0.005)\n"
            "else:\n"
            "    sys.exit(1)\n"
            "time.sleep(0.12)\n"
            "import shlex\n"
            "sh_cmd = \"sleep 0.08 && echo '[READY:TARGET]' > \" + shlex.quote(app_ready) + \" && sleep 0.5\"\n"
            "os.execv(sh_target, [sh_target, '-c', sh_cmd])\n"
        )
        child = subprocess.Popen(
            [sys.executable, "-c", launcher_code, sampler_ready_path, ready_path, sh_bin],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            result = measure_single_profile(
                attach_pid=child.pid,
                expected_exe=sh_bin,
                ready_file=ready_path,
                ready_marker="[READY:TARGET]",
                sampler_ready_file=sampler_ready_path,
                profile_label="helper exec",
                steady_seconds=0.25,
                sample_interval=0.03,
                readiness_timeout=4.0,
            )
            setup = [s for s in result["samples"] if s["phase"] == "launcher-setup"]
            self.assertGreater(len(setup), 0)
            self.assertGreaterEqual(max(s["totalRssMib"] for s in setup), 50.0)
            self.assertTrue(all(s["exe"] == py_exe for s in setup))
            self.assertGreater(result["sampleInterval"]["launchSampleCount"], 0)
            self.assertEqual(result["launchClaim"], "verified-pre-exec-gate")
            self.assertIsNotNone(result["timestamps"]["execDiscoveredMonotonic"])
            self.assertLess(result["launchMetrics"]["sampledPeakRssMib"], 25.0)
            self.assertLess(result["steadyMetrics"]["rssMedianMib"], 25.0)
            self.assertLess(result["overallPeak"]["sampledPeakRssMib"], 25.0)
            hwm = result["mainProcessVmHwm"]["mib"]
            self.assertIsNotNone(hwm)
            self.assertLess(hwm, 25.0)
            for s in result["samples"]:
                if s["phase"] in ("launch", "steady"):
                    self.assertEqual(s["exe"], sh_bin)
        finally:
            try:
                child.kill()
                child.wait(timeout=2)
            except Exception:
                pass

    def test_attach_without_expected_exe_remains_pre_ready(self) -> None:
        ready_path = os.path.join(self.temp_dir.name, "app.ready")
        child_code = (
            "import sys, time\n"
            "time.sleep(0.1)\n"
            "open(sys.argv[1], 'w').write('[READY:UNVERIFIED]\\n')\n"
            "time.sleep(0.3)\n"
        )
        child = subprocess.Popen(
            [sys.executable, "-c", child_code, ready_path],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            result = measure_single_profile(
                attach_pid=child.pid,
                ready_file=ready_path,
                ready_marker="[READY:UNVERIFIED]",
                profile_label="unverified attach",
                steady_seconds=0.2,
                sample_interval=0.03,
                readiness_timeout=3.0,
            )
            self.assertIsNone(result["launchMetrics"])
            self.assertIsNone(result["launchClaim"])
            self.assertEqual(result["sampleInterval"]["launchSampleCount"], 0)
            self.assertGreater(result["sampleInterval"]["preReadySampleCount"], 0)
            self.assertIsNotNone(result["timestamps"]["attachToReadySec"])
            self.assertIsNone(result["timestamps"]["launchDurationSec"])
        finally:
            child.kill()
            child.wait(timeout=2)

    def test_early_exit_before_exec_fails_closed(self) -> None:
        sampler_ready_path = os.path.join(self.temp_dir.name, "sampler_ready.signal")
        child_code = (
            "import os, sys, time\n"
            "ready_sig = sys.argv[1]\n"
            "for _ in range(1000):\n"
            "    if os.path.exists(ready_sig):\n"
            "        break\n"
            "    time.sleep(0.005)\n"
            "sys.exit(42)\n"
        )
        child = subprocess.Popen(
            [sys.executable, "-c", child_code, sampler_ready_path],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        previous = signal.getsignal(signal.SIGINT)
        try:
            with self.assertRaises(ProcessCrashedError) as cm:
                measure_single_profile(
                    attach_pid=child.pid,
                    expected_exe="/bin/sleep",
                    ready_file=os.path.join(self.temp_dir.name, "app.ready"),
                    ready_marker="[READY:CRASH]",
                    sampler_ready_file=sampler_ready_path,
                    profile_label="early exit",
                    steady_seconds=0.2,
                    sample_interval=0.03,
                    readiness_timeout=3.0,
                )
            self.assertIn("exited", str(cm.exception).lower())
            self.assertIn("expected", str(cm.exception).lower())
            self.assertIsNotNone(child.poll())
        finally:
            child.wait(timeout=2)
        self.assertEqual(signal.getsignal(signal.SIGINT), previous)

    def test_spawn_readiness_timeout_reaps_child(self) -> None:
        pidfile = os.path.join(self.temp_dir.name, "pid")
        code = (
            "import os, sys, time\n"
            "open(sys.argv[1], 'w').write(str(os.getpid()))\n"
            "time.sleep(30)\n"
        )
        previous = signal.getsignal(signal.SIGINT)
        with self.assertRaises(ReadinessTimeoutError):
            measure_single_profile(
                argv=[sys.executable, "-c", code, pidfile],
                ready_marker="[READY:NEVER]",
                profile_label="cancel cleanup",
                steady_seconds=1.0,
                sample_interval=0.05,
                readiness_timeout=0.6,
            )
        self.assertTrue(os.path.exists(pidfile))
        with open(pidfile, encoding="utf-8") as f:
            pid = int(f.read())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        self.assertEqual(signal.getsignal(signal.SIGINT), previous)


if __name__ == "__main__":
    unittest.main()
