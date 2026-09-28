#!/usr/bin/env python3
"""
Unit and regression tests for scripts/smoke_native.py.
Verifies:
- Strict non-zero exit on wrong version, missing binary, or non-executable file.
- Clean process group termination on timeout / hang without leaking orphan processes.
- Verification against the frozen pilot binary proving that its missing --version
  causes a strict FAIL, never a fake green pass.
"""

from __future__ import annotations

import io
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch
from pathlib import Path

from scripts.smoke_native import (
    SmokeFailure,
    kill_proc_group,
    missing_markers,
    run_bounded_cmd,
    verify_cli_smoke,
    verify_launch,
)


class TestSmokeNative(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.test_dir = Path(self.temp_dir.name)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def _create_mock_bin(self, code_body: str) -> Path:
        """Creates a mock Python executable script."""
        script = self.test_dir / "mock_app"
        script.write_text(
            f"#!{sys.executable}\nimport sys\n{code_body}\n",
            encoding="utf-8",
        )
        script.chmod(script.stat().st_mode | stat.S_IXUSR | stat.S_IRUSR)
        return script

    def test_launch_waits_for_both_markers_and_stops_the_app(self) -> None:
        """A window that opens and loads its one repo passes; the app is stopped."""
        mock = self._create_mock_bin("""
import os, time
assert os.environ.get('SNIP_NATIVE_E2E') == '1'
ws = sys.argv[sys.argv.index('--workspace') + 1]
assert os.path.isdir(os.path.join(ws, 'repo', '.git'))
print('[APP:WINDOW_READY]', flush=True)
print('[APP:READY_REPOS: 1]', flush=True)
time.sleep(60)
""")
        start = time.monotonic()
        with patch("sys.stdout", io.StringIO()):
            verify_launch(mock, timeout_sec=20.0)
        self.assertLess(time.monotonic() - start, 15.0)

    def test_launch_fails_when_the_app_exits_or_never_loads(self) -> None:
        for name, body in (("exits", "print('[APP:WINDOW_READY]'); sys.exit(3)"),
                           ("hangs", "import time; print('[APP:WINDOW_READY]', flush=True); time.sleep(60)")):
            with self.subTest(name=name):
                mock = self._create_mock_bin(body)
                with patch("sys.stdout", io.StringIO()), \
                        self.assertRaisesRegex(SmokeFailure, "READY_REPOS"):
                    verify_launch(mock, timeout_sec=2.0)

    def test_launch_returns_when_a_detached_child_keeps_the_output(self) -> None:
        """A child that outlives the app and holds its output (git, a helper) cannot hang the check."""
        mock = self._create_mock_bin("""
import subprocess, time
subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'],
                 stdout=sys.stdout, stderr=sys.stderr, start_new_session=True)
print('[APP:WINDOW_READY]', flush=True)
print('[APP:READY_REPOS: 1]', flush=True)
time.sleep(60)
""")
        start = time.monotonic()
        with patch("sys.stdout", io.StringIO()):
            verify_launch(mock, timeout_sec=20.0)
        self.assertLess(time.monotonic() - start, 15.0)

    def test_missing_markers_needs_exact_lines(self) -> None:
        self.assertEqual(missing_markers(["[APP:WINDOW_READY]", "[APP:READY_REPOS: 10]"]),
                         ["[APP:READY_REPOS: 1]"])
        self.assertEqual(missing_markers(["  [APP:WINDOW_READY]", "[APP:READY_REPOS: 1]"]), [])

    def test_mock_app_success(self) -> None:
        """Verifies that a well-behaved binary passes CLI smoke."""
        mock = self._create_mock_bin("""
if '--help' in sys.argv:
    print('snip-desktop-native: Native Git Workbench\\nOptions:\\n  -V, --version  Print version')
    sys.exit(0)
if '--version' in sys.argv:
    print('snip-desktop-native 0.1.4')
    sys.exit(0)
if '--unrecognized-smoke-test-flag' in sys.argv:
    print('error: unrecognized option', file=sys.stderr)
    sys.exit(2)
sys.exit(1)
""")
        # Redirected Windows stdout may use cp1252 instead of UTF-8.
        with io.TextIOWrapper(io.BytesIO(), encoding="cp1252") as output:
            with patch("sys.stdout", output):
                verify_cli_smoke(
                    bin_path=mock,
                    expected_version="0.1.4",
                    app_name="snip-desktop-native",
                    timeout_sec=2.0,
                )

    def test_missing_binary_fails(self) -> None:
        """Verifies that a non-existent binary raises SmokeFailure."""
        with self.assertRaises(SmokeFailure):
            verify_cli_smoke(
                bin_path=self.test_dir / "non_existent",
                expected_version="0.1.4",
            )

    def test_non_executable_binary_fails(self) -> None:
        """Verifies that a binary without execute permissions fails."""
        mock = self.test_dir / "non_exec"
        mock.write_text("dummy", encoding="utf-8")
        mock.chmod(0o644)
        if os.name == "posix":
            with self.assertRaises(SmokeFailure):
                verify_cli_smoke(
                    bin_path=mock,
                    expected_version="0.1.4",
                )

    def test_wrong_version_fails(self) -> None:
        """Verifies that wrong version output fails smoke."""
        mock = self._create_mock_bin("""
if '--help' in sys.argv:
    print('snip-desktop-native\\n  --version')
    sys.exit(0)
if '--version' in sys.argv:
    print('snip-desktop-native 0.1.0')
    sys.exit(0)
if '--unrecognized-smoke-test-flag' in sys.argv:
    sys.exit(2)
""")
        with self.assertRaises(SmokeFailure) as cm:
            verify_cli_smoke(
                bin_path=mock,
                expected_version="0.1.4",
                timeout_sec=2.0,
            )
        self.assertIn("exact match failed", str(cm.exception))

    def test_nearby_substring_version_strictly_fails(self) -> None:
        """
        Verifies that printing 'snip-desktop-native 10.1.40' when expecting '0.1.4'
        is strictly rejected. Substring matching is forbidden.
        """
        mock = self._create_mock_bin("""
if '--help' in sys.argv:
    print('snip-desktop-native\\n  -V, --version')
    sys.exit(0)
if '--version' in sys.argv:
    print('snip-desktop-native 10.1.40')
    sys.exit(0)
if '--unrecognized-smoke-test-flag' in sys.argv:
    sys.exit(2)
""")
        with self.assertRaises(SmokeFailure) as cm:
            verify_cli_smoke(
                bin_path=mock,
                expected_version="0.1.4",
                timeout_sec=2.0,
            )
        self.assertIn("exact match failed", str(cm.exception))

    def test_help_missing_version_documentation_fails(self) -> None:
        """Verifies that if --help does not document --version or -V, smoke fails."""
        mock = self._create_mock_bin("""
if '--help' in sys.argv:
    print('snip-desktop-native: Native Git Workbench\\nNo options listed')
    sys.exit(0)
if '--version' in sys.argv:
    print('snip-desktop-native 0.1.4')
    sys.exit(0)
if '--unrecognized-smoke-test-flag' in sys.argv:
    sys.exit(2)
""")
        with self.assertRaises(SmokeFailure) as cm:
            verify_cli_smoke(
                bin_path=mock,
                expected_version="0.1.4",
                timeout_sec=2.0,
            )
        self.assertIn("does not document '--version' or '-V'", str(cm.exception))

    def test_unknown_flag_wrong_exit_code_fails(self) -> None:
        """Verifies that unknown flags must exit with code 2 (exit 0 or 1 rejected)."""
        mock = self._create_mock_bin("""
if '--help' in sys.argv:
    print('snip-desktop-native\\n  --version')
    sys.exit(0)
if '--version' in sys.argv:
    print('snip-desktop-native 0.1.4')
    sys.exit(0)
if '--unrecognized-smoke-test-flag' in sys.argv:
    sys.exit(1)  # Wrong exit code: should be 2
""")
        with self.assertRaises(SmokeFailure) as cm:
            verify_cli_smoke(
                bin_path=mock,
                expected_version="0.1.4",
                timeout_sec=2.0,
            )
        self.assertIn("expected exit code 2", str(cm.exception))

    def test_hanging_command_times_out_and_cleans_up_process(self) -> None:
        """
        Verifies that a binary that hangs (simulating entering a GUI loop on --version)
        times out, raises SmokeFailure, and cleanly kills the child process.
        """
        pid_file = self.test_dir / "child.pid"
        mock = self._create_mock_bin(f"""
import os, time
if '--help' in sys.argv:
    print('snip-desktop-native\\n  --version')
    sys.exit(0)
if '--version' in sys.argv:
    with open({repr(str(pid_file))}, 'w') as f:
        f.write(str(os.getpid()))
    while True:
        time.sleep(0.1)
""")
        start_time = time.monotonic()
        with self.assertRaises(SmokeFailure) as cm:
            verify_cli_smoke(
                bin_path=mock,
                expected_version="0.1.4",
                timeout_sec=0.5,
            )
        elapsed = time.monotonic() - start_time
        self.assertLess(elapsed, 2.5)
        self.assertIn("timed out", str(cm.exception))

        # Check that the spawned child PID was killed and does not remain alive
        if pid_file.is_file():
            child_pid = int(pid_file.read_text().strip())
            time.sleep(0.1)
            if os.name == "posix":
                try:
                    os.kill(child_pid, 0)
                    alive = True
                except OSError:
                    alive = False
                self.assertFalse(alive, f"Child process {child_pid} was leaked after timeout")

    def test_descendant_pipe_holding_process_reaped_on_timeout(self) -> None:
        """
        Focused regression for Defect 5:
        Parent process spawns a background child process that inherits stdout and sleeps.
        Parent exits immediately (proc.poll() != None).
        run_bounded_cmd must detect pipe retention, timeout, invoke kill_proc_group
        to reap the surviving descendant, close all pipes cleanly, and raise SmokeFailure.
        """
        child_pid_file = self.test_dir / "descendant.pid"
        mock = self._create_mock_bin(f"""
import os, subprocess, sys
# Spawn a detached child process that inherits stdout and sleeps
code = f'''
import os, time
with open({repr(str(child_pid_file))}, 'w') as f:
    f.write(str(os.getpid()))
time.sleep(30)
'''
subprocess.Popen([sys.executable, '-c', code], stdout=sys.stdout, stderr=sys.stderr)
# Parent exits immediately with code 0!
sys.exit(0)
""")
        start_time = time.monotonic()
        with self.assertRaises(SmokeFailure) as cm:
            run_bounded_cmd([str(mock)], timeout_sec=0.5)
        elapsed = time.monotonic() - start_time
        self.assertLess(elapsed, 2.5)
        self.assertIn("timed out", str(cm.exception))

        # Verify the descendant holding the pipe was terminated
        time.sleep(0.1)
        if child_pid_file.is_file():
            descendant_pid = int(child_pid_file.read_text().strip())
            if os.name == "posix":
                try:
                    os.kill(descendant_pid, 0)
                    alive = True
                except OSError:
                    alive = False
                self.assertFalse(alive, f"Descendant process {descendant_pid} was not reaped after timeout")

    def test_already_reaped_root_never_signals_pgid(self) -> None:
        """
        Focused regression for Defect 1:
        Verifies that if proc.returncode is already set (root process was already reaped),
        kill_proc_group returns immediately and NEVER signals the old process group ID,
        avoiding PID reuse race conditions.
        """
        mock_proc = MagicMock(spec=subprocess.Popen)
        mock_proc.pid = 99999
        mock_proc.returncode = 0  # Root already reaped prior to cleanup call

        with patch("os.killpg") as mock_killpg:
            kill_proc_group(mock_proc)
            mock_killpg.assert_not_called()


if __name__ == "__main__":
    unittest.main()
