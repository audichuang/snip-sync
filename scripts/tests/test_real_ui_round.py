#!/usr/bin/env python3
"""
Unit tests for scripts/real_ui_round.py.

Tests the platform-independent logic:
- Log parsing for CTRL_BOUNDS, CTRL_GONE, and VIEWPORT
- Coordinate math for logical window and screen points across scale 1 and scale 2
- environment.json fixed schema validation
- Leftover detection and selective cleaning with fakes
- User config directory snapshot and drift detection
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import threading
import sys
import tempfile
import unittest
from typing import Any, Dict, List, Optional

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

import real_ui_round as rur


class FakeSystemOperations(rur.SystemOperations):
    """In-memory fake of OS operations for platform-independent tests."""

    def __init__(self):
        self.processes: List[rur.ProcessRecord] = []
        self.killed_pids: List[int] = []
        self.clipboard_data: bytes = b""
        self.remote_responses: Dict[str, str] = {}
        self.applescripts_run: List[str] = []
        self.registered_apps: List[Path] = []
        self.unregistered_apps: List[Path] = []

    def list_processes(self) -> List[rur.ProcessRecord]:
        return list(self.processes)

    def is_pid_alive(self, pid: int) -> bool:
        return any(p.pid == pid for p in self.processes) and pid not in self.killed_pids

    def kill_process(self, pid: int, sig: signal.Signals = signal.SIGTERM) -> None:
        self.killed_pids.append(pid)

    def wait_process_exit(self, pid: int, timeout_seconds: float = 10.0) -> Optional[int]:
        if pid in self.killed_pids or not any(p.pid == pid for p in self.processes):
            return 0
        return None

    def run_applescript(self, script: str) -> str:
        self.applescripts_run.append(script)
        return ""

    def get_clipboard_bytes(self) -> bytes:
        return self.clipboard_data

    def set_clipboard_bytes(self, data: bytes) -> None:
        self.clipboard_data = data

    def register_app(self, app_path: Path) -> None:
        self.registered_apps.append(app_path)

    def unregister_app(self, app_path: Path) -> None:
        self.unregistered_apps.append(app_path)

    def remote_exec(self, host: str, command: str, check: bool = True):
        import subprocess
        stdout = self.remote_responses.get(command, "")
        rc = 0 if command in self.remote_responses else 1
        return subprocess.CompletedProcess(args=["ssh", host, command], returncode=rc, stdout=stdout, stderr="")


class TestLogParsing(unittest.TestCase):
    def test_parse_control_bounds_finds_latest_entry(self):
        lines = [
            "[APP:CTRL_BOUNDS: id=btn-apply x=10 y=20 w=80 h=30]",
            "[APP:CTRL_BOUNDS: id=btn-other x=100 y=100 w=50 h=50]",
            "[APP:CTRL_BOUNDS: id=btn-apply x=15 y=25 w=85 h=35]",
        ]
        match = rur.parse_control_bounds_from_lines(lines, "btn-apply")
        self.assertEqual(match.control_id, "btn-apply")
        self.assertEqual(match.x, 15.0)
        self.assertEqual(match.y, 25.0)
        self.assertEqual(match.w, 85.0)
        self.assertEqual(match.h, 35.0)
        self.assertEqual(match.line_number, 3)

    def test_parse_control_bounds_respects_ctrl_gone(self):
        lines = [
            "[APP:CTRL_BOUNDS: id=btn-apply x=10 y=20 w=80 h=30]",
            "[APP:CTRL_GONE: id=btn-apply]",
        ]
        with self.assertRaisesRegex(ValueError, "missing-control btn-apply"):
            rur.parse_control_bounds_from_lines(lines, "btn-apply")

    def test_parse_control_bounds_reactivates_after_subsequent_bounds(self):
        lines = [
            "[APP:CTRL_BOUNDS: id=btn-apply x=10 y=20 w=80 h=30]",
            "[APP:CTRL_GONE: id=btn-apply]",
            "[APP:CTRL_BOUNDS: id=btn-apply x=12 y=22 w=80 h=30]",
        ]
        match = rur.parse_control_bounds_from_lines(lines, "btn-apply")
        self.assertEqual(match.x, 12.0)
        self.assertEqual(match.line_number, 3)

    def test_parse_control_bounds_missing_control_raises(self):
        lines = ["[APP:CTRL_BOUNDS: id=btn-other x=10 y=20 w=80 h=30]"]
        with self.assertRaisesRegex(ValueError, "missing-control btn-apply"):
            rur.parse_control_bounds_from_lines(lines, "btn-apply")

    def test_parse_control_bounds_zero_or_negative_dimensions_raises(self):
        lines = ["[APP:CTRL_BOUNDS: id=btn-zero x=10 y=20 w=0 h=30]"]
        with self.assertRaisesRegex(ValueError, "invalid-control-dimensions"):
            rur.parse_control_bounds_from_lines(lines, "btn-zero")

    def test_parse_latest_viewport_extracts_dimensions(self):
        lines = [
            "[APP:VIEWPORT: 900x600]",
            "some log text",
            "[APP:VIEWPORT: 1080x752]",
            "trailing text",
        ]
        vp = rur.parse_latest_viewport(lines)
        self.assertEqual(vp, (1080, 752))

    def test_parse_latest_viewport_missing_raises(self):
        lines = ["log line 1", "log line 2"]
        with self.assertRaisesRegex(ValueError, "No \\[APP:VIEWPORT: ...\\] line found"):
            rur.parse_latest_viewport(lines)


class TestAppWindowTitle(unittest.TestCase):
    def test_bare_title_matches(self):
        self.assertTrue(rur.is_app_window_title("snip-sync"))

    def test_tab_label_title_matches(self):
        self.assertTrue(rur.is_app_window_title("project-a \u2014 snip-sync"))
        self.assertTrue(rur.is_app_window_title("host \u25b8 name \u2014 snip-sync"))

    def test_other_titles_do_not_match(self):
        self.assertFalse(rur.is_app_window_title(""))
        self.assertFalse(rur.is_app_window_title("snip-sync QA abc123"))
        self.assertFalse(rur.is_app_window_title("project-a - snip-sync"))
        self.assertFalse(rur.is_app_window_title("project-a snip-sync"))
        self.assertFalse(rur.is_app_window_title("snip-syncx"))
        self.assertFalse(rur.is_app_window_title(None))


class TestPointCoordinateMath(unittest.TestCase):
    def test_calculate_point_coordinates_scale_1(self):
        # Data mirroring the real reference pending-action.json from the operator
        bounds = rur.ControlBoundsMatch(
            control_id="btn-paste",
            x=1027.0,
            y=9.0,
            w=20.0,
            h=20.0,
            line_number=25486,
            raw_text="[APP:CTRL_BOUNDS: id=btn-paste x=1027 y=9 w=20 h=20]",
        )
        viewport = (1080, 720)
        frame = {"X": 545.0, "Y": 281.0, "Width": 1080.0, "Height": 752.0}
        scale = 1.0

        res = rur.calculate_point_coordinates(bounds, viewport, frame, scale)

        self.assertEqual(res["id"], "btn-paste")
        self.assertEqual(res["titlebar"], 32.0)
        self.assertEqual(res["content_origin"], [545.0, 313.0])
        # Tool point in window space: x = (1027 + 10) / 1 = 1037, y = (9 + 10) / 1 + 32 = 51
        self.assertEqual(res["tool_point"], [1037.0, 51.0])
        # Screen point: 545 + 1037 = 1582, 313 + 19 = 332
        self.assertEqual(res["screen_point"], [1582.0, 332.0])

    def test_calculate_point_coordinates_scale_2_retina(self):
        # 2x Retina scale display
        bounds = rur.ControlBoundsMatch(
            control_id="btn-test",
            x=100.0,
            y=200.0,
            w=40.0,
            h=60.0,
            line_number=100,
            raw_text="[APP:CTRL_BOUNDS: id=btn-test x=100 y=200 w=40 h=60]",
        )
        # Logical frame 900x632 => Physical viewport width 1800, content height 600 * 2 = 1200
        viewport = (1800, 1200)
        frame = {"X": 200.0, "Y": 150.0, "Width": 900.0, "Height": 632.0}
        scale = 2.0

        res = rur.calculate_point_coordinates(bounds, viewport, frame, scale)

        # Titlebar: 632 - (1200 / 2) = 32
        self.assertEqual(res["titlebar"], 32.0)
        self.assertEqual(res["content_origin"], [200.0, 182.0])
        # Content center: x = (100 + 20) / 2 = 60.0, y = (200 + 30) / 2 = 115.0
        # Tool point: [60.0, 115.0 + 32.0] = [60.0, 147.0]
        self.assertEqual(res["tool_point"], [60.0, 147.0])
        # Screen point: [200 + 60, 182 + 115] = [260.0, 297.0]
        self.assertEqual(res["screen_point"], [260.0, 297.0])

    def test_calculate_point_coordinates_scale_mismatch_raises(self):
        bounds = rur.ControlBoundsMatch(
            control_id="btn-test", x=10.0, y=10.0, w=10.0, h=10.0, line_number=1, raw_text=""
        )
        # Frame width 1000 * scale 1 = 1000, but viewport reports 900
        viewport = (900, 600)
        frame = {"X": 0.0, "Y": 0.0, "Width": 1000.0, "Height": 632.0}
        scale = 1.0

        with self.assertRaisesRegex(ValueError, "Viewport physical width 900 does not match"):
            rur.calculate_point_coordinates(bounds, viewport, frame, scale)


class TestEnvironmentSchema(unittest.TestCase):
    def setUp(self):
        self.valid_data = {
            "sha": "292621beb4f8dd47fa3015bc14ac106bf7f2e832",
            "branch": "develop",
            "source_worktree": "/tmp/worktree",
            "build": "cargo build -p snip-desktop-native --locked (debug)",
            "binary": "/tmp/snip-sync QA.app/Contents/MacOS/snip-desktop-native",
            "binary_sha256": "abcdef1234567890",
            "app_bundle": "/tmp/snip-sync QA.app",
            "config_isolated": "/tmp/config",
            "export_hold_file": "/tmp/export-hold (absent at start)",
            "paste_id": "paste-<row|include|overwrite>:<ix>:<path>",
            "fixtures": {
                "gate_a": "/tmp/gate-a",
                "gate_b": "/tmp/gate-b/fixtures",
                "perf15": "/tmp/perf15",
            },
            "fixture_logs": [
                "fixture-gate-b.log",
                "fixture-perf15.log",
                "fixture-gate-a.log (verify exit 0)",
            ],
            "launch_env": {
                "SNIP_NATIVE_E2E": "1",
                "SNIP_THEME": "dark",
                "SNIP_CONFIG_DIR": "/tmp/config",
                "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE": "/tmp/export-hold",
            },
            "launch_gate_b": "launch gate b command",
            "launch_gate_a": "launch gate a command",
            "prepared_by": "tester",
            "viewport_measured": "(to be filled)",
        }

    def test_validate_environment_schema_valid_data(self):
        errors = rur.validate_environment_schema(self.valid_data)
        self.assertEqual(errors, [])

    def test_validate_environment_schema_missing_keys(self):
        invalid_data = dict(self.valid_data)
        del invalid_data["sha"]
        del invalid_data["binary_sha256"]
        errors = rur.validate_environment_schema(invalid_data)
        self.assertTrue(any("Missing required key 'sha'" in e for e in errors))
        self.assertTrue(any("Missing required key 'binary_sha256'" in e for e in errors))

    def test_validate_environment_schema_missing_fixture(self):
        invalid_data = dict(self.valid_data)
        invalid_data["fixtures"] = {"gate_b": "/tmp/gate-b"}
        errors = rur.validate_environment_schema(invalid_data)
        self.assertTrue(any("Missing fixture path 'gate_a'" in e for e in errors))

    def test_validate_environment_schema_missing_launch_env(self):
        invalid_data = dict(self.valid_data)
        invalid_data["launch_env"] = {"SNIP_THEME": "dark"}
        errors = rur.validate_environment_schema(invalid_data)
        self.assertTrue(any("Missing environment variable 'SNIP_CONFIG_DIR'" in e for e in errors))


class TestLeftoversDetectionAndCleaning(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.temp_dir.name)

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_detect_leftovers_empty(self):
        sys_ops = FakeSystemOperations()
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(leftovers, [])

    def test_detect_leftovers_ignores_interpreters_with_snip_args(self):
        sys_ops = FakeSystemOperations()
        sys_ops.processes = [
            rur.ProcessRecord(
                pid=999,
                command="agy -p 'You are testing snip-desktop-native today'",
            ),
            rur.ProcessRecord(
                pid=998,
                command="/usr/bin/python3 scripts/real_ui_round.py --help",
            ),
        ]
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(leftovers, [])

    def test_detect_leftovers_identifies_earlier_round_desktop_process(self):
        sys_ops = FakeSystemOperations()
        sys_ops.processes = [
            rur.ProcessRecord(
                pid=1234,
                command="/Users/user/snip-sync-ui-runs/2026-10-10-292621b/snip-sync QA.app/Contents/MacOS/snip-desktop-native",
            )
        ]
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(len(leftovers), 1)
        self.assertEqual(leftovers[0].kind, "process")
        self.assertTrue(leftovers[0].provably_earlier_round)

    def test_detect_leftovers_identifies_unprovable_desktop_process(self):
        sys_ops = FakeSystemOperations()
        sys_ops.processes = [
            rur.ProcessRecord(
                pid=5678,
                command="target/debug/snip-desktop-native --workspace /some/custom/dir",
            )
        ]
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(len(leftovers), 1)
        self.assertEqual(leftovers[0].kind, "process")
        self.assertFalse(leftovers[0].provably_earlier_round)

    def test_detect_leftovers_identifies_export_hold(self):
        sys_ops = FakeSystemOperations()
        hold = self.run_dir / "export-hold"
        hold.write_text("hold")
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(len(leftovers), 1)
        self.assertEqual(leftovers[0].kind, "export-hold")
        self.assertTrue(leftovers[0].provably_earlier_round)

    def test_clean_provable_leftovers_kills_provable_leaves_unprovable(self):
        sys_ops = FakeSystemOperations()
        sys_ops.processes = [
            rur.ProcessRecord(
                pid=1001,
                command="/Users/user/snip-sync-ui-runs/run1/snip-desktop-native",
            ),
            rur.ProcessRecord(
                pid=1002,
                command="/dev/build/snip-desktop-native",
            ),
        ]
        leftovers = rur.detect_leftovers(self.run_dir, sys_ops=sys_ops)
        self.assertEqual(len(leftovers), 2)

        remaining = rur.clean_provable_leftovers(leftovers, sys_ops=sys_ops)
        self.assertIn(1001, sys_ops.killed_pids)
        self.assertNotIn(1002, sys_ops.killed_pids)
        self.assertEqual(len(remaining), 1)
        self.assertEqual(remaining[0].clean_target, 1002)


class TestConfigSnapshot(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.config_dir = Path(self.temp_dir.name)
        (self.config_dir / "settings.json").write_text('{"theme": "dark"}')
        (self.config_dir / "sub").mkdir()
        (self.config_dir / "sub" / "data.bin").write_bytes(b"\x01\x02\x03")

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_snapshot_identical(self):
        snap = rur.snapshot_config_dir(self.config_dir)
        diffs = rur.verify_config_snapshot(self.config_dir, snap)
        self.assertEqual(diffs, [])

    def test_snapshot_detects_modification(self):
        snap = rur.snapshot_config_dir(self.config_dir)
        (self.config_dir / "settings.json").write_text('{"theme": "light"}')
        diffs = rur.verify_config_snapshot(self.config_dir, snap)
        self.assertTrue(any("Modified content: settings.json" in d for d in diffs))

    def test_snapshot_detects_addition(self):
        snap = rur.snapshot_config_dir(self.config_dir)
        (self.config_dir / "unexpected.txt").write_text("leak")
        diffs = rur.verify_config_snapshot(self.config_dir, snap)
        self.assertTrue(any("Unexpected new file: unexpected.txt" in d for d in diffs))

    def test_snapshot_detects_deletion(self):
        snap = rur.snapshot_config_dir(self.config_dir)
        (self.config_dir / "sub" / "data.bin").unlink()
        diffs = rur.verify_config_snapshot(self.config_dir, snap)
        self.assertTrue(any("Deleted file: sub/data.bin" in d for d in diffs))


class TestClipboardOperations(unittest.TestCase):
    def test_clipboard_save_and_restore_fake(self):
        sys_ops = FakeSystemOperations()
        sys_ops.set_clipboard_bytes(b"initial clipboard bytes \x00\xff")
        self.assertEqual(sys_ops.get_clipboard_bytes(), b"initial clipboard bytes \x00\xff")

        # Mutate
        sys_ops.set_clipboard_bytes(b"mutated during tests")
        self.assertEqual(sys_ops.get_clipboard_bytes(), b"mutated during tests")

        # Restore
        sys_ops.set_clipboard_bytes(b"initial clipboard bytes \x00\xff")
        self.assertEqual(sys_ops.get_clipboard_bytes(), b"initial clipboard bytes \x00\xff")


class TestPasteIdDetection(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_detects_indexed_paste_id(self):
        paste_rs = self.root / "crates" / "desktop-native" / "src" / "paste.rs"
        paste_rs.parent.mkdir(parents=True, exist_ok=True)
        paste_rs.write_text('/// probe id: `paste-<kind>:<ix>:<path>`\nenum RowControl {}')
        self.assertEqual(
            rur.detect_paste_id_format(self.root),
            "paste-<row|include|overwrite>:<ix>:<path>",
        )

    def test_detects_legacy_paste_id(self):
        paste_rs = self.root / "crates" / "desktop-native" / "src" / "paste.rs"
        paste_rs.parent.mkdir(parents=True, exist_ok=True)
        paste_rs.write_text('/// legacy format without ix\n')
        self.assertEqual(rur.detect_paste_id_format(self.root), "paste-row:<path>")


class RoundDirTestCase(unittest.TestCase):
    """A prepared run directory whose app binary exits at once."""

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.temp_dir.name)
        self.sys_ops = FakeSystemOperations()

        # Create mock binary: a python script that exits 0
        self.mock_bin = self.run_dir / "mock_app"
        self.mock_bin.write_text(f"""#!/bin/sh
echo "[APP:VIEWPORT: 900x600]"
exit 0
""")
        self.mock_bin.chmod(0o755)

        # Setup fixtures
        fixtures_dir = self.run_dir / "gate-b" / "fixtures" / "ws-src"
        fixtures_dir.mkdir(parents=True, exist_ok=True)
        (self.run_dir / "gate-a" / "machine-a").mkdir(parents=True, exist_ok=True)
        (self.run_dir / "config").mkdir(parents=True, exist_ok=True)

        self.env_data = {
            "sha": "9dffa2e80715e47fec3dd29563414ce933082b4a",
            "branch": "feature/test",
            "source_worktree": str(self.run_dir / "worktree"),
            "build": "cargo build",
            "binary": str(self.mock_bin),
            "binary_sha256": "abc123",
            "app_bundle": str(self.run_dir / "snip-sync QA test.app"),
            "config_isolated": str(self.run_dir / "config"),
            "export_hold_file": str(self.run_dir / "export-hold"),
            "paste_id": "paste-row:<path>",
            "fixtures": {
                "gate_b": str(self.run_dir / "gate-b" / "fixtures"),
                "perf15": str(self.run_dir / "perf15"),
                "gate_a": str(self.run_dir / "gate-a"),
            },
            "fixture_logs": ["fixture.log"],
            "launch_env": {
                "SNIP_NATIVE_E2E": "1",
                "SNIP_THEME": "dark",
                "SNIP_CONFIG_DIR": str(self.run_dir / "config"),
                "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE": str(self.run_dir / "export-hold"),
            },
            "launch_gate_b": "launch",
            "launch_gate_a": "launch",
            "prepared_by": "test",
            "viewport_measured": {},
            "round_script_sha256": rur.script_sha256(),
        }
        (self.run_dir / "environment.json").write_text(json.dumps(self.env_data, indent=2))

    def tearDown(self):
        self.temp_dir.cleanup()


class TestLaunchSupervisorAndExitFile(RoundDirTestCase):
    def test_supervisor_records_exit_code_and_times(self):
        args = argparse.Namespace(gate="b", run=str(self.run_dir))
        rc = rur.cmd_supervisor(args)
        self.assertEqual(rc, 0)

        # Check app-process.json was created
        proc_file = self.run_dir / "app-process.json"
        self.assertTrue(proc_file.is_file())
        proc_data = json.loads(proc_file.read_text(encoding="utf-8"))
        self.assertIn("pid", proc_data)
        self.assertIn("supervisor_pid", proc_data)
        self.assertEqual(proc_data["gate"], "b")

        # Check app-gate-b-exit.json was created
        exit_file = self.run_dir / "app-gate-b-exit.json"
        self.assertTrue(exit_file.is_file())
        exit_data = json.loads(exit_file.read_text(encoding="utf-8"))
        self.assertEqual(exit_data["exit_code"], 0)
        self.assertIsNone(exit_data["signal"])
        self.assertIn("started_at", exit_data)
        self.assertIn("finished_at", exit_data)

    def test_finish_fails_when_exit_receipt_is_missing(self):
        # Create app-process.json without exit receipt
        proc_data = {"pid": 99999, "gate": "b", "log": str(self.run_dir / "app-gate-b.log")}
        (self.run_dir / "app-process.json").write_text(json.dumps(proc_data))

        args = argparse.Namespace(run=str(self.run_dir))
        rc = rur.cmd_finish(args, sys_ops=self.sys_ops)
        self.assertEqual(rc, 1)

    def test_finish_fails_when_app_exited_nonzero(self):
        proc_data = {"pid": 99999, "gate": "b", "log": str(self.run_dir / "app-gate-b.log")}
        (self.run_dir / "app-process.json").write_text(json.dumps(proc_data))

        exit_data = {
            "pid": 99999,
            "exit_code": 1,
            "signal": None,
            "started_at": "2026-10-10T00:00:00Z",
            "finished_at": "2026-10-10T00:00:01Z",
        }
        (self.run_dir / "app-gate-b-exit.json").write_text(json.dumps(exit_data))

        args = argparse.Namespace(run=str(self.run_dir))
        rc = rur.cmd_finish(args, sys_ops=self.sys_ops)
        self.assertEqual(rc, 1)

    def test_finish_succeeds_when_exit_receipt_is_zero(self):
        proc_data = {"pid": 99999, "gate": "b", "log": str(self.run_dir / "app-gate-b.log")}
        (self.run_dir / "app-process.json").write_text(json.dumps(proc_data))

        exit_data = {
            "pid": 99999,
            "exit_code": 0,
            "signal": None,
            "started_at": "2026-10-10T00:00:00Z",
            "finished_at": "2026-10-10T00:00:01Z",
        }
        (self.run_dir / "app-gate-b-exit.json").write_text(json.dumps(exit_data))

        # Setup dummy bundle path so finish does not fail on leftover process or unregister
        app_bundle = Path(self.env_data["app_bundle"])
        app_bundle.mkdir(parents=True, exist_ok=True)

        args = argparse.Namespace(run=str(self.run_dir))
        rc = rur.cmd_finish(args, sys_ops=self.sys_ops)
        self.assertEqual(rc, 0)
        self.assertIn(app_bundle, self.sys_ops.unregistered_apps)

        # Check recorded finish_results
        env_res = json.loads((self.run_dir / "environment.json").read_text(encoding="utf-8"))
        self.assertEqual(env_res["finish_results"]["exit_code"], 0)


class TestBundleIsolation(unittest.TestCase):
    def test_bundle_contains_launcher_and_lsenvironment(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            macos_dir = run_dir / "MyApp.app" / "Contents" / "MacOS"
            macos_dir.mkdir(parents=True, exist_ok=True)

            dest_bin = macos_dir / "snip-desktop-native"
            dest_bin.write_text("#!/bin/sh\nexit 0\n")
            dest_bin.chmod(0o755)

            isolated_config = run_dir / "config"
            isolated_config.mkdir(parents=True, exist_ok=True)

            launcher_script = macos_dir / "snip-desktop-launcher"
            launcher_content = f"""#!/bin/sh
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

            self.assertTrue(launcher_script.is_file())
            self.assertTrue(os.access(launcher_script, os.X_OK))

            script_text = launcher_script.read_text(encoding="utf-8")
            self.assertIn(f"SNIP_CONFIG_DIR:-{isolated_config}", script_text)
            self.assertIn("SNIP_NATIVE_E2E:-1", script_text)
            self.assertIn(f'exec "{dest_bin}"', script_text)

            # Plist checks
            info_plist = run_dir / "MyApp.app" / "Contents" / "Info.plist"
            bundle_id = "com.snipsync.qa.test"
            plist_content = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>snip-desktop-launcher</string>
	<key>CFBundleIdentifier</key>
	<string>{bundle_id}</string>
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
            info_plist.write_text(plist_content, encoding="utf-8")
            plist_text = info_plist.read_text(encoding="utf-8")
            self.assertIn("<key>CFBundleExecutable</key>\n\t<string>snip-desktop-launcher</string>", plist_text)
            self.assertIn("<key>LSEnvironment</key>", plist_text)
            self.assertIn(f"<string>{isolated_config}</string>", plist_text)


HELPER = {"kCGWindowName": "Window", "kCGWindowLayer": 0, "kCGWindowOwnerPID": 42,
          "kCGWindowBounds": {"X": 0, "Y": 0, "Width": 10, "Height": 10}}


def main_window(title: str) -> Dict[str, Any]:
    return {"kCGWindowName": title, "kCGWindowLayer": 0, "kCGWindowOwnerPID": 42,
            "kCGWindowBounds": {"X": 100, "Y": 50, "Width": 1080, "Height": 752}}


class TestFindMainWindow(unittest.TestCase):
    def test_skips_the_helper_window_listed_first(self):
        info = {"windows": [HELPER, main_window("ws-src — snip-sync")]}
        self.assertEqual(rur.find_main_window(info, 42)["kCGWindowName"], "ws-src — snip-sync")

    def test_bare_title_without_tabs(self):
        info = {"windows": [main_window("snip-sync"), HELPER]}
        self.assertEqual(rur.find_main_window(info, 42)["kCGWindowName"], "snip-sync")

    def test_ignores_other_pids_and_other_layers(self):
        other = dict(main_window("ws-src — snip-sync"), kCGWindowOwnerPID=7)
        overlay = dict(main_window("x — snip-sync"), kCGWindowLayer=3)
        info = {"windows": [other, overlay, main_window("alpha — snip-sync")]}
        self.assertEqual(rur.find_main_window(info, 42)["kCGWindowName"], "alpha — snip-sync")

    def test_no_match_lists_every_title_of_the_pid(self):
        info = {"windows": [HELPER, main_window("alpha - snip-sync")]}
        with self.assertRaises(rur.MainWindowError) as ctx:
            rur.find_main_window(info, 42)
        self.assertIn("'Window'", str(ctx.exception))
        self.assertIn("'alpha - snip-sync'", str(ctx.exception))

    def test_nameless_windows_point_at_screen_recording(self):
        nameless = {k: v for k, v in main_window("x").items() if k != "kCGWindowName"}
        with self.assertRaises(rur.MainWindowError) as ctx:
            rur.find_main_window({"windows": [nameless]}, 42)
        self.assertIn("Screen Recording", str(ctx.exception))

    def test_two_main_windows_are_an_error(self):
        info = {"windows": [main_window("a — snip-sync"), main_window("b — snip-sync")]}
        with self.assertRaises(rur.MainWindowError):
            rur.find_main_window(info, 42)

    def test_applescript_string_escapes_quotes_and_backslashes(self):
        self.assertEqual(rur.applescript_string('a "b" \\ c'), '"a \\"b\\" \\\\ c"')


class WindowFakeSystemOperations(FakeSystemOperations):
    def __init__(self, windows: List[Dict[str, Any]]):
        super().__init__()
        self.windows = windows
        self.processes = [rur.ProcessRecord(pid=42, command="snip-desktop-native")]

    def get_window_info(self, pid: int, helper_binary: Optional[Path] = None) -> Dict[str, Any]:
        return {"screens": [{"frame": [0, 0, 1920, 1080], "scale": 1.0}], "windows": list(self.windows)}


class TestWindowCommands(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.temp_dir.name)
        self.log = self.run_dir / "app-gate-b.log"
        self.log.write_text(
            "[APP:VIEWPORT: 1080x720]\n[APP:CTRL_BOUNDS: id=ws-tab:0 x=10 y=4 w=100 h=26]\n", encoding="utf-8"
        )
        (self.run_dir / "app-process.json").write_text(json.dumps({"pid": 42, "gate": "b", "log": str(self.log)}))
        self.write_env(rur.script_sha256())
        self.sys_ops = WindowFakeSystemOperations([HELPER, main_window('say "hi" — snip-sync')])

    def tearDown(self):
        self.temp_dir.cleanup()

    def write_env(self, script_sha: Optional[str]) -> None:
        env = {"sha": "516cb1c" + "0" * 33}
        if script_sha is not None:
            env["round_script_sha256"] = script_sha
        (self.run_dir / "environment.json").write_text(json.dumps(env))

    def run_quiet(self, fn, args):
        import contextlib
        import io
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = fn(args, sys_ops=self.sys_ops)
        return rc, out.getvalue(), err.getvalue()

    def test_title_prints_the_main_window_not_the_helper(self):
        rc, out, _ = self.run_quiet(rur.cmd_title, argparse.Namespace(run=str(self.run_dir)))
        self.assertEqual(rc, 0)
        self.assertEqual(out, 'say "hi" — snip-sync\n')

    def test_point_finds_a_window_titled_with_the_tab_label(self):
        rc, out, err = self.run_quiet(
            rur.cmd_point, argparse.Namespace(run=str(self.run_dir), control_id="ws-tab:0", hover=None)
        )
        self.assertEqual(rc, 0, err)
        self.assertIn("Screen Point", out)

    def test_resize_targets_the_exact_main_window_title(self):
        rc, _, err = self.run_quiet(rur.cmd_resize, argparse.Namespace(run=str(self.run_dir), width=1080, height=720))
        self.assertEqual(rc, 0, err)
        self.assertEqual(len(self.sys_ops.applescripts_run), 1)
        self.assertIn('set size of window "say \\"hi\\" — snip-sync" to', self.sys_ops.applescripts_run[0])

    def test_point_and_title_refuse_a_script_other_than_the_prepared_one(self):
        for sha in (None, "0" * 64):
            self.write_env(sha)
            rc, _, err = self.run_quiet(rur.cmd_title, argparse.Namespace(run=str(self.run_dir)))
            self.assertEqual(rc, 1)
            self.assertIn("not the script this round was prepared with", err)
            rc, _, _ = self.run_quiet(
                rur.cmd_point, argparse.Namespace(run=str(self.run_dir), control_id="ws-tab:0", hover=None)
            )
            self.assertEqual(rc, 1)


WRAPPER = '#!/bin/sh\necho $$ > "/home/audichuang/snip-ui-run/516cb1c/pids/$$"\nexec x "$@"\n'


class RemoteFake(FakeSystemOperations):
    """A remote host with files and a scripted stop_run_workers result."""

    def __init__(self, files: Dict[str, str], stop_rc: int = 0):
        super().__init__()
        self.files = dict(files)
        self.stop_rc = stop_rc
        self.commands: List[str] = []

    def remote_exec(self, host: str, command: str, check: bool = True):
        self.commands.append(command)
        rc, out = 0, ""
        if command.startswith("W=") and "bash -c" in command:
            rc = self.stop_rc
        elif command.startswith("cat '"):
            out = "111\n"
        elif command.startswith("sha256sum "):
            path = command.split()[1]
            if path in self.files:
                out = f"{rur.hashlib.sha256(self.files[path].encode()).hexdigest()}  {path}\n"
        elif command.startswith("rm -f "):
            self.files.pop(command.split()[2], None)
        elif command.startswith("if [ -e "):
            backup = command.split()[3]
            if backup in self.files and rur.REMOTE_WRAPPER not in self.files:
                self.files[rur.REMOTE_WRAPPER] = self.files.pop(backup)
        elif command.startswith("[ -e "):
            rc = 0 if command.split()[2] in self.files else 1
        elif command.startswith("cat ") and command.split()[1] in self.files:
            out = self.files[command.split()[1]]
        return subprocess.CompletedProcess(args=["ssh", host, command], returncode=rc, stdout=out, stderr="")


class TestRemoteCleanup(unittest.TestCase):
    SHA = "516cb1c" + "0" * 33
    BACKUP = "~/.local/bin/snip.uirun-516cb1c"

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.temp_dir.name)
        digest = rur.hashlib.sha256(WRAPPER.encode()).hexdigest()
        (self.run_dir / "snip-installed.sha").write_text(f"{digest}  /home/audichuang/.local/bin/snip\n")

    def tearDown(self):
        self.temp_dir.cleanup()

    def clean(self, fake: RemoteFake) -> Dict[str, Any]:
        import contextlib
        import io
        with contextlib.redirect_stderr(io.StringIO()):
            return rur.clean_remote_round("ubuntu", self.SHA, self.run_dir, fake)

    def assert_no_glob(self, fake: RemoteFake) -> None:
        for command in fake.commands:
            self.assertNotIn("uirun-*", command)

    def test_removes_only_this_rounds_wrapper_backup_and_dir(self):
        other = "~/.local/bin/snip.uirun-1234567"
        fake = RemoteFake({rur.REMOTE_WRAPPER: WRAPPER, other: "user backup"})
        result = self.clean(fake)
        self.assertTrue(result["workers_stopped"])
        self.assertEqual(result["wrapper"], "removed")
        self.assertEqual(fake.files, {other: "user backup"})
        self.assertIn("rm -rf '/home/audichuang/snip-ui-run/516cb1c'", fake.commands)
        self.assertEqual((self.run_dir / "workers-at-cleanup.txt").read_text(), "111\n")
        self.assert_no_glob(fake)

    def test_a_replaced_wrapper_is_kept(self):
        fake = RemoteFake({rur.REMOTE_WRAPPER: "#!/bin/sh\nexec /usr/bin/snip \"$@\"\n"})
        result = self.clean(fake)
        self.assertIn(rur.REMOTE_WRAPPER, fake.files)
        self.assertTrue(result["wrapper"].startswith("kept"))
        self.assertNotIn(f"rm -f {rur.REMOTE_WRAPPER}", fake.commands)

    def test_an_interrupted_s11_backup_is_restored_then_removed(self):
        fake = RemoteFake({self.BACKUP: WRAPPER})
        result = self.clean(fake)
        self.assertEqual(fake.files, {})
        self.assertEqual(result["wrapper"], "removed")
        self.assertEqual(result["backup"], "absent")

    def test_a_backup_with_other_content_is_kept(self):
        fake = RemoteFake({rur.REMOTE_WRAPPER: WRAPPER, self.BACKUP: "something else"})
        self.clean(fake)
        self.assertEqual(fake.files, {self.BACKUP: "something else"})

    def test_failed_worker_stop_keeps_everything(self):
        fake = RemoteFake({rur.REMOTE_WRAPPER: WRAPPER}, stop_rc=1)
        result = self.clean(fake)
        self.assertFalse(result["workers_stopped"])
        self.assertEqual(fake.files, {rur.REMOTE_WRAPPER: WRAPPER})
        self.assertFalse(any(c.startswith(("rm ", "if ")) for c in fake.commands))

    def test_stop_command_passes_the_run_dir(self):
        command = rur.stop_run_workers_command("/home/audichuang/snip-ui-run/516cb1c")
        self.assertTrue(command.startswith("W=/home/audichuang/snip-ui-run/516cb1c bash -c '"))

    def test_leftover_check_looks_only_at_this_shas_backup(self):
        other = "~/.local/bin/snip.uirun-1234567"
        fake = RemoteFake({other: WRAPPER, self.BACKUP: WRAPPER})
        leftovers = rur.detect_leftovers(self.run_dir, remote_host="ubuntu", sys_ops=fake, short_sha="516cb1c")
        self.assertEqual([item.clean_target for item in leftovers], [self.BACKUP])
        self.assertTrue(leftovers[0].provably_earlier_round)
        rur.clean_provable_leftovers(leftovers, remote_host="ubuntu", sys_ops=fake)
        self.assertEqual(fake.files, {other: WRAPPER})
        self.assert_no_glob(fake)


class TestFinishWithRemote(RoundDirTestCase):
    def test_finish_fails_and_keeps_the_remote_dir_when_workers_do_not_stop(self):
        env = json.loads((self.run_dir / "environment.json").read_text())
        env["remote_host"] = "ubuntu"
        (self.run_dir / "environment.json").write_text(json.dumps(env))
        Path(self.env_data["app_bundle"]).mkdir(parents=True, exist_ok=True)
        fake = RemoteFake({rur.REMOTE_WRAPPER: WRAPPER}, stop_rc=1)
        import contextlib
        import io
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            rc = rur.cmd_finish(argparse.Namespace(run=str(self.run_dir)), sys_ops=fake)
        self.assertEqual(rc, 1)
        self.assertFalse(any(c.startswith("rm -rf") for c in fake.commands))
        recorded = json.loads((self.run_dir / "environment.json").read_text())
        self.assertFalse(recorded["finish_results"]["remote_cleanup"]["workers_stopped"])


@unittest.skipIf(os.name == "nt", "the worker cleanup runs in bash on the Linux worker")
class TestStopRunWorkersScript(unittest.TestCase):
    """Runs section 2.3's stop_run_workers against real processes; a fake `ps`
    reports the worker command line for PIDs listed in $FAKE_ARGS."""

    def setUp(self):
        assert shutil.which("bash"), "bash is required"
        self.temp_dir = tempfile.TemporaryDirectory()
        root = Path(self.temp_dir.name)
        self.w = root / "W"
        (self.w / "pids").mkdir(parents=True)
        self.args_dir = root / "args"
        self.args_dir.mkdir()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        fake_ps = bin_dir / "ps"
        fake_ps.write_text(
            '#!/bin/sh\npid="$4"\nkill -0 "$pid" 2>/dev/null || exit 1\n'
            '[ -f "$FAKE_ARGS/$pid" ] && cat "$FAKE_ARGS/$pid" || echo "sleep 60"\n'
        )
        fake_ps.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", FAKE_ARGS=str(self.args_dir))
        self.procs: List[subprocess.Popen] = []

    def tearDown(self):
        for proc in self.procs:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=10)
        self.temp_dir.cleanup()

    def worker(self, args: Optional[str]) -> int:
        proc = subprocess.Popen(["sleep", "60"])
        self.procs.append(proc)
        # Reap it as sshd reaps a worker, or a killed one lingers as a zombie.
        threading.Thread(target=proc.wait, daemon=True).start()
        (self.w / "pids" / str(proc.pid)).write_text(f"{proc.pid}\n")
        if args is not None:
            (self.args_dir / str(proc.pid)).write_text(args + "\n")
        return proc.pid

    def run_stop(self) -> subprocess.CompletedProcess:
        command = rur.stop_run_workers_command(str(self.w))
        return subprocess.run(["sh", "-c", command], env=self.env, capture_output=True, text=True, timeout=60)

    def test_stops_recorded_workers_and_skips_exited_ones(self):
        pid = self.worker(f"{self.w}/src/target/release/snip serve --stdio")
        (self.w / "pids" / "999999").write_text("999999\n")
        result = self.run_stop()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"stopped {pid}", result.stdout)
        self.assertIsNotNone(self.procs[0].wait(timeout=10))

    def test_refuses_a_pid_whose_arguments_differ(self):
        pid = self.worker(None)
        result = self.run_stop()
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"PID {pid} arguments differ", result.stderr)
        self.assertIsNone(self.procs[0].poll())

    def test_rejects_an_invalid_record(self):
        (self.w / "pids" / "bad").write_text("12; rm -rf /\n")
        result = self.run_stop()
        self.assertEqual(result.returncode, 1)
        self.assertIn("invalid PID record", result.stderr)


if __name__ == "__main__":
    unittest.main()
