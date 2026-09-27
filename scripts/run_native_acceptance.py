#!/usr/bin/env python3
"""Build once, pin current inputs, and run the existing Linux acceptance drivers.

All outputs are fresh and retained. --build-receipt reuses a build made by this
entrypoint only when the checkout and frozen executable still match it.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import importlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from bench_tauri_memory import sha256_file
from check_native_ime import REQUIRED_TOOLS as IME_TOOLS

ROOT = Path(__file__).resolve().parent.parent
BUILD = ["cargo", "build", "--release", "-p", "snip-desktop-native", "--locked",
         "--message-format=json-render-diagnostics"]
GATES = ("ime", "collaboration", "resource-short")
PRODUCER = "run_native_acceptance.py/v1"


def environment() -> dict[str, str]:
    env = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "DISPLAY",
                "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(key, None)
    env.update(SNIP_REQUIRE_ALL_TESTS="1", PYTHONDONTWRITEBYTECODE="1")
    return env


def git(*args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], env=environment())


def source_snapshot() -> dict:
    """Include dirty/staged/untracked inputs, not just a possibly misleading HEAD."""
    files = {}
    paths = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    for raw in sorted(set(paths.split(b"\0")) - {b""}):
        name = os.fsdecode(raw)
        path = ROOT / name
        if path.is_symlink():
            content = os.fsencode(os.readlink(path))
            files[name] = {"link": hashlib.sha256(content).hexdigest()}
            if path.is_file():
                files[name]["sha256"] = sha256_file(str(path))
            elif path.is_dir():
                raise ValueError(f"unsupported source directory symlink: {path}")
        elif path.is_file():
            files[name] = {"sha256": sha256_file(str(path)),
                           "executable": bool(path.stat().st_mode & 0o111)}
        elif not path.exists():
            files[name] = {"missing": True}
        else:
            raise ValueError(f"unsupported source input: {path}")
    return {"head": git("rev-parse", "HEAD").decode().strip(),
            "tree": git("rev-parse", "HEAD^{tree}").decode().strip(),
            "status": git("status", "--porcelain=v1", "-z", "--untracked-files=all").decode(),
            "files": files}


def write_json(path: Path, data: dict) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run(command: list[str], output: Path, name: str, commands: list[dict]) -> None:
    log = output / f"{name}.log"
    record = {"command": command, "log": str(log), "exitCode": None}
    commands.append(record)
    print(f"{name}: {log}", flush=True)
    with log.open("w") as stream:
        result = subprocess.run(command, cwd=ROOT, env=environment(),
                                stdout=stream, stderr=subprocess.STDOUT)
    record["exitCode"] = result.returncode
    if result.returncode:
        raise RuntimeError(f"{name} failed with exit {result.returncode}; see {log}")


def fresh_output(requested: Path | None) -> Path:
    if requested is None:
        parent = ROOT / "target" / "native-acceptance"
        parent.mkdir(parents=True, exist_ok=True)
        return Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    if requested.exists() or requested.is_symlink():
        raise ValueError("--output must not exist; historical evidence is never replaced")
    output = requested.resolve()
    if output.is_relative_to(ROOT):
        ignored = subprocess.run(["git", "-C", str(ROOT), "check-ignore", "-q", str(output)],
                                 env=environment()).returncode
        if ignored != 0:
            raise ValueError("output inside the checkout must be gitignored (use target/)")
    output.mkdir(parents=True)
    return output


def prerequisites(gates: tuple[str, ...], building: bool) -> None:
    if sys.platform != "linux" or sys.byteorder != "little":
        raise ValueError("native acceptance requires little-endian Linux; refusing to skip")
    tools = {"git"}
    if building:
        tools.update(("cargo", "rustc"))
    if gates:
        tools.update(("Xvfb", "xdotool", "xclip", "xwd", "convert", "dbus-daemon",
                      "dbus-run-session", "xwininfo"))
    if "ime" in gates:
        tools.update(IME_TOOLS)
        tools.add("cc")
        try:
            importlib.import_module("PIL.Image")
        except ImportError as exc:
            raise ValueError("Pillow is required; select an interpreter with SNIP_NATIVE_PYTHON") from exc
    missing = sorted(name for name in tools if shutil.which(name) is None)
    if missing:
        raise ValueError("required tools missing: " + ", ".join(missing))


def build(output: Path, commands: list[dict]) -> Path:
    before = source_snapshot()
    try:
        run(BUILD, output, "build", commands)
    finally:
        after = source_snapshot()
        write_json(output / "build-inputs.json", {"before": before, "after": after})
    if after != before:
        raise ValueError("source changed during build; refusing to issue a receipt")
    artifacts = []
    for line in (output / "build.log").read_text().splitlines():
        if not line.startswith("{"):
            continue
        row = json.loads(line)
        if (row.get("reason") == "compiler-artifact"
                and row.get("target", {}).get("name") == "snip-desktop-native"
                and row.get("executable")):
            artifacts.append(Path(row["executable"]))
    if len(artifacts) != 1:
        raise ValueError("build did not identify exactly one native executable")
    binary = output / "snip-desktop-native"
    shutil.copy2(artifacts[0], binary)
    receipt = output / "build-receipt.json"
    write_json(receipt, {
        "producer": PRODUCER, "source": before, "sourceSha": before["head"],
        "binary": str(binary), "sha256": sha256_file(str(binary)), "buildProfile": "release",
        "buildCommand": BUILD, "buildCwd": str(ROOT), "buildExitCode": 0,
        "buildLog": str(output / "build.log"),
        "buildEnvironment": {key: os.environ.get(key) for key in
                             ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                              "CARGO_TARGET_DIR", "LIBRARY_PATH", "RUSTUP_TOOLCHAIN")},
        "createdAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    })
    return receipt


def verify_build(receipt: Path) -> dict:
    data = json.loads(receipt.read_text())
    if (not isinstance(data, dict) or data.get("producer") != PRODUCER or data.get("buildCommand") != BUILD
            or data.get("buildExitCode") != 0 or data.get("buildProfile") != "release"):
        raise ValueError("receipt is not a successful release build from this entrypoint")
    binary = Path(data["binary"])
    if not binary.is_absolute() or not os.access(binary, os.X_OK):
        raise ValueError("receipt executable is missing, relative, or not executable")
    if sha256_file(str(binary)) != data.get("sha256"):
        raise ValueError("frozen executable changed since the build")
    if source_snapshot() != data.get("source") or data.get("sourceSha") != data["source"]["head"]:
        raise ValueError("checkout head/tree/dirty state or source files changed since the build")
    return data


def run_gate(gate: str, output: Path, receipt: Path, data: dict, commands: list[dict]) -> None:
    python = [sys.executable, "-B"]
    binary, sha = data["binary"], data["sha256"]
    if gate == "ime":
        run(python + ["scripts/check_native_ime_startup.py", "--binary", binary,
                      "--output", str(output / "ime")], output, gate, commands)
    elif gate == "collaboration":
        fixture = output / "collaboration-fixture"
        run(python + ["scripts/collaboration_fixture.py", "generate", "--output", str(fixture)],
            output, "collaboration-fixture", commands)
        manifest = json.loads((fixture / "manifest.json").read_text())
        helpers = output / "helper-receipt.json"
        write_json(helpers, {"files": {name: {"sha256": sha256_file(str(ROOT / "scripts" / name))}
                                     for name in ("bench_native_memory.py", "bench_tauri_memory.py",
                                                  "memory_harness.py")}})
        run(python + ["scripts/check_native_collaboration.py", "--binary", binary,
                      "--binary-sha", sha, "--fixture", str(fixture),
                      "--dataset-hash", manifest["datasetHash"], "--helper-receipt", str(helpers),
                      "--phase", "all", "--timeout", "60", "--output", str(output / gate)],
            output, gate, commands)
    else:
        profile = gate.removeprefix("resource-")
        fixture = output / f"workload-{profile}"
        run(python + ["scripts/workload_generator.py", "--target-dir", str(fixture),
                      "--preset", "standard" if profile == "long" else "medium"],
            output, f"workload-{profile}", commands)
        run(python + ["scripts/check_native_leaks.py", "--profile", profile, "--bin", binary,
                      "--expected-binary-sha256", sha, "--workspace", str(fixture),
                      "--expected-fixture-sha256", sha256_file(str(fixture / "workload_manifest.json")),
                      "--expected-source-sha", data["sourceSha"], "--build-receipt", str(receipt),
                      "--build-profile", "release", "--label",
                      "standard long gate" if profile == "long" else "functional short subgate (medium fixture; not full D4)",
                      "--out-dir", str(output / gate)], output, gate, commands)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gate", choices=("build", "all", *GATES, "resource-long"), default="all")
    parser.add_argument("--output", type=Path, help="Fresh directory (default: unique target/native-acceptance run)")
    parser.add_argument("--build-receipt", type=Path, help="Reuse this entrypoint's frozen build, verifying all inputs")
    args = parser.parse_args(argv)
    output = None
    summary = {"gate": args.gate, "commands": [], "status": "FAILED", "fullD4Claimed": False}
    try:
        gates = GATES if args.gate == "all" else (() if args.gate == "build" else (args.gate,))
        output = fresh_output(args.output)
        print(f"Acceptance evidence: {output}", flush=True)
        prerequisites(gates, building=args.build_receipt is None)
        receipt = args.build_receipt.resolve() if args.build_receipt else build(output, summary["commands"])
        summary["buildReceipt"] = str(receipt)
        for gate in gates:
            data = verify_build(receipt)
            try:
                run_gate(gate, output, receipt, data, summary["commands"])
            finally:
                verify_build(receipt)
        verify_build(receipt)
        summary["status"] = "PASSED"
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as exc:
        summary["error"] = str(exc)
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    finally:
        if output is not None:
            write_json(output / "acceptance.json", summary)


if __name__ == "__main__":
    sys.exit(main())
