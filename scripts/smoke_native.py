#!/usr/bin/env python3
"""
scripts/smoke_native.py

Strict, portable CLI smoke runner for native desktop binary across Linux, macOS, and Windows.
Uses Python stdlib subprocess with process group timeouts (no GNU timeout dependency).
Enforces:
- Explicit binary path and expected version (NO default fallback to pilot).
- Strict non-zero exit on wrong version, command failure, or timeout/hang.
- Clean process group termination on timeout (zero leaked processes).
- Explicit labeling as CLI-only smoke (real GUI remains unverified on macOS/Windows).

--launch additionally opens the real window on a one-repo workspace and waits
for the app's own markers: proof that the packaged GPUI backend starts, renders
and reads Git on this OS. It sends no input.
"""

from __future__ import annotations

import argparse
import os
import queue
import shutil
import signal
import subprocess
import tempfile
import threading
import sys
import time
from pathlib import Path
from typing import List, Optional, Tuple


class SmokeFailure(Exception):
    """Raised when any CLI smoke condition fails."""


def kill_proc_group(proc: subprocess.Popen) -> None:
    """
    Safely terminates owned process group and cleans up pipes.
    On POSIX:
    - If proc.returncode is already set, root has already been reaped; never signal old PGID (avoids PID reuse race).
    - Otherwise, signals the owned process group BEFORE any poll/wait/reap while root identity is still held.
      Since start_new_session=True, pgid is guaranteed to equal proc.pid without lookup fallback.
    - Bounded wait reaps the root process.
    On Windows:
    - Limitation: taskkill cannot guarantee discovering detached descendants if the root process has already exited;
      bounded timeout in run_bounded_cmd ensures the CLI check fails safely without hanging.
    """
    if proc.returncode is not None:
        # Root was already reaped prior to this call; never signal old process group ID
        return

    pid = proc.pid

    if os.name == "posix":
        # Kill owned process group BEFORE any poll/wait/reap while root identity is held.
        # With start_new_session=True, pgid is guaranteed to equal proc.pid.
        try:
            os.killpg(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    else:
        # Windows: bounded taskkill while root process is un-reaped.
        # Note: taskkill /T cannot guarantee terminating detached child processes if the parent
        # has already exited; timeout expiration in run_bounded_cmd handles this as a test failure.
        try:
            subprocess.run(
                ["taskkill", "/F", "/T", "/PID", str(pid)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=2.0,
                check=False,
            )
        except Exception:
            pass

    try:
        proc.wait(timeout=1.0)
    except Exception:
        pass


def run_bounded_cmd(
    argv: List[str],
    timeout_sec: float = 4.0,
) -> Tuple[int, str, str]:
    """
    Executes a command with a strict timeout and process group isolation.
    Returns (exit_code, stdout, stderr).
    Ensures all pipes are closed in a finally block to prevent ResourceWarning leaks.
    Raises SmokeFailure on timeout.
    """
    kwargs = {
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "text": True,
    }
    if os.name == "posix":
        kwargs["start_new_session"] = True

    try:
        proc = subprocess.Popen(argv, **kwargs)
    except FileNotFoundError as e:
        raise SmokeFailure(f"Executable not found: {argv[0]} ({e})") from e
    except PermissionError as e:
        raise SmokeFailure(f"Permission denied executing: {argv[0]} ({e})") from e

    try:
        stdout, stderr = proc.communicate(timeout=timeout_sec)
        return proc.returncode, stdout, stderr
    except subprocess.TimeoutExpired as e:
        kill_proc_group(proc)
        raise SmokeFailure(
            f"Command {argv} timed out after {timeout_sec}s (likely entered GUI loop or child retained pipes)"
        ) from e
    finally:
        # Explicitly close all streams to avoid ResourceWarnings and file descriptor leaks
        for stream in (proc.stdout, proc.stderr, proc.stdin):
            if stream is not None and not getattr(stream, "closed", True):
                try:
                    stream.close()
                except Exception:
                    pass


def verify_cli_smoke(
    bin_path: Path,
    expected_version: str,
    app_name: str = "snip-desktop-native",
    timeout_sec: float = 4.0,
) -> None:
    """
    Strictly verifies native CLI behavior. Fails on any deviation.
    Enforces:
    - Exit 0 on --help with application banner AND documentation for --version / -V.
    - Exit 0 on --version with EXACT expected line '{app_name} {clean_ver}' (per UI contract).
    - Exit 2 on unknown flags (standard CLI syntax error rejection).
    """
    print(f"=== Native CLI Smoke (CLI Interface Only) ===")
    print(f"Target binary: {bin_path}")
    print(f"Expected version: {expected_version}")
    print(f"Platform: {sys.platform} ({os.uname().machine if hasattr(os, 'uname') else 'unknown'})")
    print(
        "Note: Real GUI window / input E2E on Linux is gated by native-smoke (Xvfb+lavapipe).\n"
        "      On macOS and Windows, --launch opens the real window (no input is sent)."
    )

    # 1. Existence and permissions
    if not bin_path.is_file():
        raise SmokeFailure(f"Binary file does not exist: {bin_path}")

    if os.name == "posix" and not os.access(bin_path, os.X_OK):
        raise SmokeFailure(f"Binary is not marked executable: {bin_path}")

    clean_ver = expected_version.lstrip("v")

    # 2. Help invocation
    print("[1/3] Checking --help...")
    code, out, err = run_bounded_cmd([str(bin_path), "--help"], timeout_sec=timeout_sec)
    if code != 0:
        raise SmokeFailure(f"'--help' failed with exit code {code}.\nStdout: {out}\nStderr: {err}")

    combined_help = out + err
    if app_name.lower() not in combined_help.lower():
        raise SmokeFailure(
            f"'--help' output missing expected application name '{app_name}'.\nOutput:\n{combined_help}"
        )
    if not ("--version" in combined_help or "-V" in combined_help):
        raise SmokeFailure(
            f"'--help' output does not document '--version' or '-V'.\nOutput:\n{combined_help}"
        )
    print("  [OK] --help returned status 0 with valid application banner and --version documentation.")

    # 3. Version invocation
    print(f"[2/3] Checking --version (expected: {clean_ver})...")
    code, out, err = run_bounded_cmd([str(bin_path), "--version"], timeout_sec=timeout_sec)
    if code != 0:
        raise SmokeFailure(
            f"'--version' failed with exit code {code}.\nStdout: {out}\nStderr: {err}"
        )

    expected_line = f"{app_name} {clean_ver}"
    lines = [ln.strip() for ln in (out + err).splitlines() if ln.strip()]
    if expected_line not in lines:
        raise SmokeFailure(
            f"'--version' output exact match failed: expected line '{expected_line}', got:\n{(out + err).strip()}"
        )
    print(f"  [OK] --version returned status 0 with exact matching output: '{expected_line}'")

    # 4. Unknown argument rejection (MUST exit with code 2)
    print("[3/3] Checking unknown flag rejection (must exit 2)...")
    code, out, err = run_bounded_cmd(
        [str(bin_path), "--unrecognized-smoke-test-flag"],
        timeout_sec=timeout_sec,
    )
    if code != 2:
        raise SmokeFailure(
            f"Unknown flag rejection failed: expected exit code 2 (syntax error), got {code}.\nStdout: {out}\nStderr: {err}"
        )
    print("  [OK] Unknown flag rejected with exit code 2.")

    print("=== Native CLI Smoke PASSED ===")


LAUNCH_MARKERS = ("[APP:WINDOW_READY]", "[APP:READY_REPOS: 1]")


def missing_markers(lines: List[str]) -> List[str]:
    """The launch markers not yet printed, in order."""
    return [m for m in LAUNCH_MARKERS if not any(line.strip() == m for line in lines)]


def one_repo_workspace(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir(parents=True)
    ident = ["-c", "user.name=snip-smoke", "-c", "user.email=smoke@example.invalid"]
    for args in (["init", "-q", "-b", "main"], ["add", "."], ["commit", "-q", "-m", "base"]):
        if args[0] == "add":
            (repo / "README.md").write_text("smoke\n", encoding="utf-8")
        subprocess.run(["git", *ident, *args], cwd=repo, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return root


def verify_launch(bin_path: Path, timeout_sec: float = 60.0) -> None:
    """Opens the real window on a one-repo workspace and waits for its markers."""
    print("[launch] Opening the real window on a one-repo workspace...")
    work = Path(tempfile.mkdtemp(prefix="snip-launch-smoke-"))
    kwargs = {"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT, "text": True,
              "env": dict(os.environ, SNIP_NATIVE_E2E="1")}
    if os.name == "posix":
        kwargs["start_new_session"] = True
    proc = None
    try:
        ws = one_repo_workspace(work / "ws")
        proc = subprocess.Popen([str(bin_path), "--workspace", str(ws)], **kwargs)
        lines: List[str] = []
        stream: "queue.Queue[Optional[str]]" = queue.Queue()

        def pump() -> None:
            for line in proc.stdout:
                stream.put(line.rstrip("\n"))
            stream.put(None)

        threading.Thread(target=pump, daemon=True).start()
        deadline = time.monotonic() + timeout_sec
        while missing_markers(lines):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeFailure(f"timed out after {timeout_sec}s waiting for {missing_markers(lines)}; "
                                   f"output:\n" + "\n".join(lines[-40:]))
            try:
                line = stream.get(timeout=min(remaining, 0.5))
            except queue.Empty:
                continue
            if line is None:
                raise SmokeFailure(f"app exited ({proc.wait()}) before {missing_markers(lines)}; "
                                   f"output:\n" + "\n".join(lines[-40:]))
            lines.append(line)
        print(f"  [OK] window opened and loaded the workspace: {', '.join(LAUNCH_MARKERS)}")
    finally:
        if proc is not None:
            kill_proc_group(proc)
            if proc.stdout is not None:
                proc.stdout.close()
        shutil.rmtree(work, ignore_errors=True)


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Strict native CLI smoke test for snip-desktop-native."
    )
    parser.add_argument(
        "--bin",
        type=Path,
        required=True,
        help="Path to the native binary to test (mandatory, no fallback).",
    )
    parser.add_argument(
        "--expected-version",
        required=True,
        help="Expected version string (e.g. 0.1.4 or v0.1.4).",
    )
    parser.add_argument(
        "--app-name",
        default="snip-desktop-native",
        help="Expected application name in help banner.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=4.0,
        help="Timeout in seconds for CLI invocations.",
    )
    parser.add_argument(
        "--launch",
        action="store_true",
        help="Also open the real window on a one-repo workspace and wait for its ready markers.",
    )
    parser.add_argument(
        "--launch-timeout",
        type=float,
        default=60.0,
        help="Seconds to wait for the launch markers.",
    )
    return parser.parse_args(argv)


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    try:
        verify_cli_smoke(
            bin_path=args.bin,
            expected_version=args.expected_version,
            app_name=args.app_name,
            timeout_sec=args.timeout,
        )
        if args.launch:
            verify_launch(args.bin, timeout_sec=args.launch_timeout)
        return 0
    except SmokeFailure as e:
        print(f"\n[FAIL] Native CLI Smoke FAILED: {e}", file=sys.stderr)
        return 1
    except Exception as e:
        print(f"\n[ERROR] Unexpected smoke failure: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
