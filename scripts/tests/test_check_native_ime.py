#!/usr/bin/env python3
"""Tests for native IME decisions and private session startup.

These do not open a display. The X11 script is the OS proof.
"""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts import check_native_ime as ime

from scripts.check_native_ime import (
    associated_candidate,
    bright_popups,
    decide_run,
    rect,
    shutdown_graceful,
)


class Bitmap:
    """The three Pillow calls `bright_popups` makes, so this suite stays stdlib-only."""

    def __init__(self, width: int, height: int) -> None:
        self.size = (width, height)
        self._lit: dict[tuple[int, int], tuple[int, int, int]] = {}

    def putpixel(self, xy: tuple[int, int], colour: tuple[int, int, int]) -> None:
        self._lit[xy] = colour

    def convert(self, mode: str) -> "Bitmap":
        assert mode == "RGB", mode
        return self

    def load(self) -> "Bitmap":
        return self

    def __getitem__(self, xy: tuple[int, int]) -> tuple[int, int, int]:
        return self._lit.get(xy, (0, 0, 0))


class TestImeGate(unittest.TestCase):
    def test_private_bus_starts_with_long_evidence_path_and_cleans_its_socket(self):
        missing = ime.missing_tools(("dbus-daemon", "dbus-send"))
        if sys.platform != "linux" or missing:
            reason = f"private D-Bus regression requires Linux and D-Bus tools: {missing}"
            self.assertNotIn("SNIP_REQUIRE_ALL_TESTS", os.environ, reason)
            self.skipTest(reason)
        with tempfile.TemporaryDirectory(prefix="snip-ime-dbus-", dir="/tmp") as directory:
            sessions, addresses, sockets, ids = [], [], [], []
            try:
                for name in ("short", "long-" + "x" * 120):
                    run = Path(directory) / name
                    run.mkdir()
                    with mock.patch.object(ime, "RUN", run), \
                            mock.patch.object(ime, "lavapipe_icd", return_value="/test/lvp.json"):
                        session = ime.Session()
                        sessions.append(session)
                        session.prepare_env()
                    self.assertEqual(session.env["HOME"], str(run / "iso" / "home"))
                    self.assertEqual(Path(session._bus_config).parent, run / "iso")
                    session.start_dbus()
                    address = session.env["DBUS_SESSION_BUS_ADDRESS"]
                    addresses.append(address)
                    if address.startswith("unix:path="):
                        sockets.append(Path(address.split(",", 1)[0].removeprefix("unix:path=")))
                    reply = subprocess.run(
                        ["dbus-send", f"--bus={address}", "--type=method_call", "--print-reply",
                         "--dest=org.freedesktop.DBus", "/", "org.freedesktop.DBus.GetId"],
                        env=session.env, capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(reply.returncode, 0, reply.stderr)
                    ids.append(reply.stdout.split('string "', 1)[1].split('"', 1)[0])
                self.assertEqual(len(set(addresses)), 2)
                self.assertEqual(len(set(ids)), 2)
            finally:
                cleanup = [(session, session.stop()) for session in sessions]
                for session, steps in cleanup:
                    self.assertTrue(ime.shutdown_graceful(steps), steps)
                    self.assertFalse(any(ime.same_process(pid, start) for pid, start, _ in session.owned))
                self.assertTrue(all(not path.exists() for path in sockets))

    def test_private_session_preserves_only_caller_library_paths(self):
        for supplied in ({}, {"LIBRARY_PATH": "/caller/build", "LD_LIBRARY_PATH": "/caller/runtime"}):
            with self.subTest(supplied=supplied), tempfile.TemporaryDirectory() as directory:
                with mock.patch.object(ime, "RUN", Path(directory)), \
                        mock.patch.object(ime, "lavapipe_icd", return_value="/test/lvp.json"), \
                        mock.patch.dict(os.environ, supplied, clear=True):
                    session = ime.Session()
                    session.prepare_env()
                for key in ("LIBRARY_PATH", "LD_LIBRARY_PATH"):
                    self.assertEqual(session.env.get(key), supplied.get(key))
                self.assertEqual(session.env["HOME"], str(Path(directory) / "iso" / "home"))

    def test_missing_tools_fail_closed_when_required(self) -> None:
        self.assertEqual(decide_run(["Xvfb"], True), "fail")
        self.assertEqual(decide_run(["fcitx5"], False), "unsupported")
        self.assertEqual(decide_run([], True), "run")

    def test_probe_geometry_is_window_bottom_not_caret(self) -> None:
        window = rect(102, 90, 1080, 720)
        field = rect(220, 562, 170, 22)
        popup = rect(104, 822, 322, 60)
        verdict = associated_candidate(popup, field, window)
        self.assertFalse(verdict["anchored"])
        self.assertTrue(verdict["windowBottomFallback"])
        self.assertGreater(verdict["dyFromFieldBottom"], 200)
        self.assertEqual(verdict["dyFromWindowBottom"], 12)

    def test_popup_just_below_the_field_is_anchored(self) -> None:
        window = rect(102, 90, 1080, 720)
        field = rect(220, 562, 170, 22)
        popup = rect(216, 586, 280, 48)
        verdict = associated_candidate(popup, field, window)
        self.assertTrue(verdict["anchored"])
        self.assertFalse(verdict["windowBottomFallback"])

    def test_resize_that_leaves_the_popup_behind_is_not_anchored(self) -> None:
        window = rect(102, 90, 900, 600)
        field = rect(220, 442, 170, 22)
        popup = rect(104, 822, 322, 60)
        verdict = associated_candidate(popup, field, window)
        self.assertFalse(verdict["anchored"])

    def test_bright_bar_is_measured_from_pixels(self) -> None:
        image = Bitmap(160, 100)
        for y in range(60, 88):
            for x in range(12, 140):
                image.putpixel((x, y), (250, 250, 250))
        found = bright_popups(image)
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0]["y"], 60)
        self.assertGreaterEqual(found[0]["w"], 80)

    def test_settle_returns_first_value_meeting_the_condition(self) -> None:
        values = iter([None, None, {"y": 1}, {"y": 2}])
        with mock.patch.object(ime.time, "sleep"):
            self.assertEqual(ime._settle(lambda: next(values), lambda v: v is not None), {"y": 1})

    def test_sigkill_is_not_called_graceful(self) -> None:
        self.assertFalse(
            shutdown_graceful(
                [
                    {"label": "app", "result": "exited"},
                    {"label": "fcitx5", "result": "sigkill"},
                ]
            )
        )
        self.assertTrue(
            shutdown_graceful(
                [
                    {"label": "app", "result": "exited"},
                    {"label": "fcitx5", "result": "exited-after-sigterm"},
                ]
            )
        )
        self.assertFalse(shutdown_graceful([]))


if __name__ == "__main__":
    unittest.main()
