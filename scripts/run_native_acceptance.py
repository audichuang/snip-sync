#!/usr/bin/env python3
"""Build once, pin current inputs, and run the existing Linux acceptance drivers.

All outputs are fresh, outside the checkout, and retained. --build-receipt reuses
a build made by this entrypoint only when the checkout and executable still match.
CI runs the gates as parallel shards against one build, then --gate merge folds
their receipts into the single gate "all" receipt a release checks.
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

from bench_native_memory import sha256_file
import check_native_collaboration as collaboration
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


def run(command: list[str], output: Path, name: str, commands: list[dict],
        ok: tuple[int, ...] = (0,)) -> None:
    log = output / f"{name}.log"
    record = {"command": command, "log": str(log), "exitCode": None}
    commands.append(record)
    print(f"{name}: {log}", flush=True)
    with log.open("w") as stream:
        result = subprocess.run(command, cwd=ROOT, env=environment(),
                                stdout=stream, stderr=subprocess.STDOUT)
    record["exitCode"] = result.returncode
    if result.returncode not in ok:
        raise RuntimeError(f"{name} failed with exit {result.returncode}; see {log}")


def fresh_output(requested: Path | None) -> Path:
    if requested is None:
        return Path(tempfile.mkdtemp(prefix="snip-native-acceptance-", dir="/tmp"))
    if requested.exists() or requested.is_symlink():
        raise ValueError("--output must not exist; historical evidence is never replaced")
    output = requested.resolve()
    if output.is_relative_to(ROOT):
        raise ValueError("--output must be outside the checkout; fixture generation refuses worktree paths")
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


def snapshot_changes(before: dict, after: dict) -> list[str]:
    """Names what differs, so "checkout changed" says which file did it."""
    if not isinstance(before, dict):
        return ["receipt has no source snapshot"]
    changes = [f"{key}: {before.get(key)} -> {after.get(key)}" for key in ("head", "tree")
               if before.get(key) != after.get(key)]
    old, new = before.get("files") or {}, after.get("files") or {}
    changes += [f"added {name}" for name in sorted(set(new) - set(old))]
    changes += [f"removed {name}" for name in sorted(set(old) - set(new))]
    changes += [f"changed {name}" for name in sorted(set(old) & set(new)) if old[name] != new[name]]
    if not changes and before.get("status") != after.get("status"):
        changes.append("git status changed")
    return changes


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
    now = source_snapshot()
    if now != data.get("source") or data.get("sourceSha") != data["source"]["head"]:
        changes = snapshot_changes(data.get("source"), now)
        raise ValueError("checkout head/tree/dirty state or source files changed since the build: "
                         + "; ".join(changes[:10]) + (f" (+{len(changes) - 10} more)" if len(changes) > 10 else ""))
    return data


def parse_shard(text: str) -> tuple[int, int]:
    """"K/N" with 1 <= K <= N."""
    try:
        k, n = (int(part) for part in text.split("/"))
    except ValueError:
        raise argparse.ArgumentTypeError("shard must be K/N, e.g. 1/2") from None
    if not 1 <= k <= n:
        raise argparse.ArgumentTypeError("shard must satisfy 1 <= K <= N")
    return k, n


def shard_steps(manifest: dict, shard: tuple[int, int]) -> list[str]:
    k, n = shard
    return [str(step["id"]) for step in manifest["steps"]][k - 1::n]


def run_gate(gate: str, output: Path, receipt: Path, data: dict, commands: list[dict],
             shard: tuple[int, int] | None = None) -> None:
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
                                     for name in ("bench_native_memory.py", "memory_harness.py")}})
        steps = shard_steps(manifest, shard) if shard else None
        select = ["--steps", ",".join(steps)] if steps else ["--phase", "all"]
        # A shard's own report always fails the driver's 18-step rule (exit 1),
        # so the shard is judged by the same checks narrowed to its steps.
        run(python + ["scripts/check_native_collaboration.py", "--binary", binary,
                      "--binary-sha", sha, "--fixture", str(fixture),
                      "--dataset-hash", manifest["datasetHash"], "--helper-receipt", str(helpers),
                      *select, "--timeout", "60", "--output", str(output / gate)],
            output, gate, commands, ok=(0, 1) if steps else (0,))
        if steps:
            report = json.loads((output / gate / "report.json").read_text())
            problems = collaboration.functional_problems(report, required_ids=steps)
            if problems:
                raise RuntimeError("collaboration shard failed: " + "; ".join(problems))
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


def merge(shards: list[Path], receipt: Path, data: dict) -> list[dict]:
    """Fold shard receipts into one "all" verdict, refusing any gap.

    Every shard must have PASSED against this very build, together they must
    cover every gate, and the collaboration shards must have passed every
    manifest step exactly once, so a lost or skipped shard cannot go green.
    """
    commands, gates, reports = [], set(), []
    for shard in shards:
        acc = json.loads((shard / "acceptance.json").read_text())
        if acc.get("status") != "PASSED":
            raise ValueError(f"{shard}: shard status is {acc.get('status')}")
        if acc.get("binarySha256") != data["sha256"] or acc.get("sourceSha") != data["sourceSha"]:
            raise ValueError(f"{shard}: shard ran a different build than {receipt}")
        commands += acc.get("commands", [])
        gates.update(acc.get("gates", []))
        if "collaboration" in acc.get("gates", []):
            reports.append(json.loads((shard / "collaboration" / "report.json").read_text()))
    missing = sorted(set(GATES) - gates)
    if missing:
        raise ValueError("no shard ran gate(s): " + ", ".join(missing))
    # One report as an unsharded run would have written it, judged by the
    # driver's own full check: all 18 steps, once each, with their evidence.
    for key in ("binarySha256", "datasetHash"):
        if len({report.get(key) for report in reports}) != 1:
            raise ValueError(f"collaboration shards disagree on {key}")
    combined = dict(reports[0], steps=[step for report in reports for step in report.get("steps", [])])
    problems = collaboration.functional_problems(combined)
    if problems:
        raise ValueError("merged collaboration report fails: " + "; ".join(problems))
    return commands


def parse_gates(text: str) -> tuple[str, ...]:
    if text in ("all", "build", "merge", "resource-long"):
        return {"all": GATES, "build": (), "merge": (), "resource-long": ("resource-long",)}[text]
    gates = tuple(part.strip() for part in text.split(","))
    unknown = [gate for gate in gates if gate not in GATES]
    if unknown or len(set(gates)) != len(gates):
        raise argparse.ArgumentTypeError(f"unknown or repeated gate(s) in {text!r}")
    return gates


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gate", default="all",
                        help="all, build, merge, resource-long, or a comma list of: " + ", ".join(GATES))
    parser.add_argument("--output", type=Path, help="Fresh directory outside checkout (default: unique /tmp run)")
    parser.add_argument("--build-receipt", type=Path, help="Reuse this entrypoint's frozen build, verifying all inputs")
    parser.add_argument("--collaboration-shard", type=parse_shard, metavar="K/N",
                        help="Run only every N-th collaboration step starting at K")
    parser.add_argument("--merge", type=Path, nargs="+", metavar="SHARD_DIR",
                        help="With --gate merge: shard outputs to fold into one gate-all receipt")
    args = parser.parse_args(argv)
    output = None
    # A merged receipt is the one a release checks; it claims the whole gate.
    summary = {"gate": "all" if args.gate == "merge" else args.gate, "commands": [],
               "status": "FAILED", "fullD4Claimed": False}
    try:
        gates = parse_gates(args.gate)
        if (args.gate == "merge") != bool(args.merge):
            raise ValueError("--merge goes with --gate merge, and only with it")
        if args.gate == "merge" and not args.build_receipt:
            raise ValueError("--gate merge needs the shards' --build-receipt")
        if args.collaboration_shard and "collaboration" not in gates:
            raise ValueError("--collaboration-shard needs the collaboration gate")
        output = fresh_output(args.output)
        print(f"Acceptance evidence: {output}", flush=True)
        prerequisites(gates, building=args.build_receipt is None)
        receipt = args.build_receipt.resolve() if args.build_receipt else build(output, summary["commands"])
        summary["buildReceipt"] = str(receipt)
        data = verify_build(receipt)
        summary.update(gates=list(gates), binarySha256=data["sha256"], sourceSha=data["sourceSha"])
        if args.collaboration_shard:
            summary["collaborationShard"] = "%d/%d" % args.collaboration_shard
        if args.merge:
            summary["commands"] = merge(args.merge, receipt, data)
            summary["gates"] = list(GATES)
            summary["shards"] = [str(path) for path in args.merge]
        for gate in gates:
            data = verify_build(receipt)
            try:
                run_gate(gate, output, receipt, data, summary["commands"], args.collaboration_shard)
            finally:
                verify_build(receipt)
        verify_build(receipt)
        summary["status"] = "PASSED"
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError,
            argparse.ArgumentTypeError, json.JSONDecodeError) as exc:
        summary["error"] = str(exc)
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    finally:
        if output is not None:
            write_json(output / "acceptance.json", summary)

if __name__ == "__main__":
    sys.exit(main())
