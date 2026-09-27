#!/usr/bin/env python3
"""Deterministic X11 startup regression, followed by the full IME acceptance.

Example: uv run --with pillow python scripts/check_native_ime_startup.py \
    --binary target/release/snip-desktop-native --output /tmp/ime-startup-fresh

A test-only libxcb shim holds CONNECT_REPLY until the real search field has
been clicked. There is no startup delay and no protocol request is suppressed.
The existing acceptance owns the private display, bus, HOME, and PID cleanup.
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import check_native_ime as ime


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if sys.platform != "linux" or sys.byteorder != "little":
        required = "SNIP_REQUIRE_ALL_TESTS" in os.environ
        print("FAIL" if required else "UNSUPPORTED", "startup probe needs little-endian Linux")
        return 2 if required else 0
    if args.output is not None and args.output.exists():
        parser.error("--output must be a fresh directory; existing evidence is never deleted")
    ime.RUN = args.output or Path(tempfile.mkdtemp(prefix="snip-ime-startup-"))
    original_session = ime.Session
    original_exercise = ime._exercise
    source = Path(__file__).parent / "tests" / "ime_startup_gate.c"
    with tempfile.TemporaryDirectory(prefix="snip-ime-shim-") as build:
        shim = Path(build) / "gate.so"

        class StartupSession(original_session):
            def start_app(self, binary: Path) -> None:
                try:
                    compile_result = subprocess.run(
                        ["cc", "-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror",
                         str(source), "-o", str(shim), "-ldl"],
                        capture_output=True, text=True,
                    )
                except OSError as exc:
                    raise ime.ImeCheckError(f"cannot build XCB probe: {exc}") from exc
                if compile_result.returncode:
                    raise ime.ImeCheckError(f"cannot build XCB probe: {compile_result.stderr}")
                original_env = self.env.copy()
                self.env["LD_PRELOAD"] = str(shim)
                self.env["SNIP_IME_STARTUP_GATE"] = str(self.run / "release-connect")
                try:
                    super().start_app(binary)
                finally:
                    # Only the native process gets the shim, never Xvfb/Fcitx/xdotool.
                    self.env = original_env

        def exercise(session, result):
            win = session.window()
            session.focus(win["wid"])

            def held():
                # Wake X11 while its handshake progresses; do not click yet.
                session.x("xdotool", "mousemove_relative", "--", "1", "0")
                return any("[IME_STARTUP] held" in line for line in session.stderr_lines)

            if not ime._wait(held, 10):
                raise ime.ImeCheckError("probe did not intercept CONNECT_REPLY")
            before = len(session.stderr_lines)
            session.click_bounds(win, ime.parse_bounds(session.texts())["log-search-input"])
            if not ime._wait(lambda: any("[IME_TRACE] anchor" in line
                                        for line in session.stderr_lines[before:]), 5):
                raise ime.ImeCheckError("input click did not reach the app during handshake")
            (session.run / "release-connect").touch()
            result["startupOrdering"] = "CONNECT_REPLY held until search input click was handled"
            try:
                code = original_exercise(session, result)
            finally:
                result["startupProtocol"] = [line for line in session.stderr_lines
                                             if line.startswith("[IME_STARTUP]")]
            if not any("released CONNECT_REPLY" in line for line in result["startupProtocol"]):
                raise ime.ImeCheckError("probe did not release CONNECT_REPLY")
            if any("premature IC" in line for line in result["startupProtocol"]):
                raise ime.ImeCheckError("app sent IC requests before handshake completed")
            return code

        ime.Session = StartupSession
        ime._exercise = exercise
        return ime.main(["--binary", str(args.binary.resolve())])


if __name__ == "__main__":
    sys.exit(main())
