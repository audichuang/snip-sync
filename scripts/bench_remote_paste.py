#!/usr/bin/env python3
"""Matched protocol-6/7 PasteTarget benchmark, without changing SSH config.

Build the same source's `snip` and `bench_paste` Cargo example first. This
measures the library paste path, not CLI argument parsing or clipboard access.
Framed bytes exclude the preamble and SSH encryption/handshake overhead.
RSS is a per-PID sampled peak, not a cumulative RUSAGE_CHILDREN high-water mark.
Corpora use the product formatter; parsed entries and output files must match
an independent normalized source manifest before measurements are accepted.
"""
from __future__ import annotations

import argparse
import ctypes
import functools
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shlex
import signal
import statistics
import struct
import subprocess
import sys
import threading
import time
import uuid
import zlib

FRAME_MAX = 8 * 1024 * 1024
INTERVAL = 0.005
PREAMBLE = b"snip-serve-stdio/1\n"
PLAN_OPS = {"import_plan", "prepare_import", "chunk", "frame_chunk"}


def atomic_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_suffix(".pending")
    pending.write_text(json.dumps(value, ensure_ascii=False, indent=2))
    pending.replace(path)


def sha(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def hashes(root: Path) -> dict[str, str]:
    return {str(p.relative_to(root)): sha(p) for p in sorted(root.rglob("*")) if p.is_file()}


class TaskInfo(ctypes.Structure):
    # macOS SDK sys/proc_info.h: proc_taskinfo / PROC_PIDTASKINFO = 4.
    _fields_ = [(name, ctypes.c_uint64) for name in
                ("virtual", "resident", "user", "system", "threads_user", "threads_system")]
    _fields_ += [(f"counter_{i}", ctypes.c_int32) for i in range(12)]


class BsdInfo(ctypes.Structure):
    _fields_ = [(f"header_{i}", ctypes.c_uint32) for i in range(12)]
    _fields_ += [("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32)]
    _fields_ += [(f"tail_{i}", ctypes.c_uint32) for i in range(6)]
    _fields_ += [("start_seconds", ctypes.c_uint64), ("start_microseconds", ctypes.c_uint64)]


@functools.lru_cache(maxsize=1)
def darwin_process_api():
    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
    clock = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    timebase = Timebase()
    if clock.mach_timebase_info(ctypes.byref(timebase)) or not timebase.denom:
        raise RuntimeError("cannot read Darwin Mach timebase")
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    lib.proc_pidinfo.restype = ctypes.c_int
    # proc_taskinfo CPU values are absolute Mach ticks, not nanoseconds.
    return lib, timebase.numer / timebase.denom / 1_000_000_000


def process_sample(pid: int):
    """Own CPU and RSS of exactly pid; child processes are never accumulated."""
    try:
        if sys.platform.startswith("linux"):
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return {"cpu_seconds": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
                    "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE")}
        if sys.platform == "darwin":
            lib, seconds_per_tick = darwin_process_api()
            info = TaskInfo()
            size = lib.proc_pidinfo(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info))
            if size != ctypes.sizeof(info):
                return None
            return {"cpu_seconds": (info.user + info.system) * seconds_per_tick,
                    "rss_bytes": info.resident}
    except (OSError, ValueError, IndexError):
        return None
    if sys.platform not in ("darwin", "linux"):
        raise RuntimeError("per-process benchmark telemetry requires Linux /proc or macOS libproc")
    return None


def process_identity(pid: int):
    try:
        if sys.platform.startswith("linux"):
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return {"pid": pid, "start": fields[19], "zombie": fields[0] in ("Z", "X")}
        if sys.platform == "darwin":
            lib, _ = darwin_process_api()
            info = BsdInfo()
            if lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info)) == ctypes.sizeof(info):
                return {"pid": pid, "start": [info.start_seconds, info.start_microseconds], "zombie": info.header_1 == 5}
            # libproc can decline a zombie even while kill(pid, 0) still
            # succeeds. Its parent will reap it after the result barrier.
            state = subprocess.run(["ps", "-p", str(pid), "-o", "stat="],
                                   text=True, capture_output=True, timeout=2).stdout.strip()
            if state.startswith("Z"):
                return {"pid": pid, "start": None, "zombie": True}
    except FileNotFoundError:
        return None
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return None
    return {"pid": pid, "start": None, "zombie": False}


def still_owned(identity):
    current = process_identity(identity["pid"])
    if current is None or current["zombie"]:
        return False
    # Unknown identity is neither evidence of exit nor authorization to kill.
    if identity["start"] is None or current["start"] is None:
        return True
    return current["start"] == identity["start"]


def finish_metrics(directory: Path):
    """Stop only this run's recorded, still-matching process identities."""
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "shutdown").touch()
    registrations = [json.loads(p.read_text()) for p in directory.glob("process-*.json")]
    if not registrations:
        return {"completed": False, "registrations": [], "signals": [],
                "error": "no owned-process registry; exit cannot be confirmed"}
    identities = [r[key] for r in registrations for key in ("child", "bridge")]
    sent = []
    for sig, grace in [(None, 5), (signal.SIGTERM, 2), (signal.SIGKILL, 2)]:
        if sig is not None:
            for identity in identities:
                current = process_identity(identity["pid"])
                if current and not current["zombie"] and identity["start"] is not None and current["start"] == identity["start"]:
                    try:
                        os.kill(identity["pid"], sig)
                        sent.append({"pid": identity["pid"], "signal": int(sig)})
                    except ProcessLookupError:
                        pass
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            if not any(still_owned(identity) for identity in identities):
                return {"completed": True, "registrations": registrations, "signals": sent}
            time.sleep(0.02)
    return {"completed": False, "registrations": registrations, "signals": sent,
            "still_alive": [identity for identity in identities if still_owned(identity)]}


class Sampler:
    def __init__(self, pid: int):
        self.pid = pid
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.baseline = 0.0
        self.peak = 0
        self.samples = 0
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self):
        while not self.stop_event.is_set():
            self.sample()
            self.stop_event.wait(INTERVAL)

    def sample(self):
        value = process_sample(self.pid)
        if value:
            with self.lock:
                self.peak = max(self.peak, value["rss_bytes"])
                self.samples += 1
        return value

    def reset(self, cold=False):
        value = self.sample()
        with self.lock:
            self.baseline = 0.0 if cold or value is None else value["cpu_seconds"]
            self.peak = 0 if value is None else value["rss_bytes"]
            self.samples = 0

    def result(self):
        value = self.sample()
        with self.lock:
            return {"pid": self.pid,
                    "cpu_seconds": None if value is None else max(0.0, value["cpu_seconds"] - self.baseline),
                    "sampled_peak_rss_bytes": self.peak, "samples": self.samples,
                    "sample_interval_seconds": INTERVAL}

    def close(self):
        self.stop_event.set()
        self.thread.join(timeout=2)


def read_exact(stream, size: int):
    chunks = []
    remaining = size
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            if not chunks:
                return None
            raise EOFError("truncated protocol frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(stream):
    prefix = read_exact(stream, 4)
    if prefix is None:
        return None
    length = struct.unpack(">I", prefix)[0]
    if length > FRAME_MAX:
        raise ValueError(f"oversized frame: {length}")
    body = read_exact(stream, length)
    if body is None:
        raise EOFError("missing frame body")
    return body


def frame_kind(body: bytes, direction: str) -> str:
    # Serde puts the discriminant first. Avoid decoding megabytes of source
    # just to meter bytes: the bridge's observer CPU must stay small.
    field = b"op" if direction == "up" else b"reply"
    found = re.search(rb'"' + field + rb'"\s*:\s*"([^"\\]+)"', body[:256])
    if not found:
        raise ValueError("missing frame discriminant")
    return found[1].decode("ascii")


def bridge(config_path: Path):
    cfg = json.loads(config_path.read_text())
    process = subprocess.Popen(cfg["argv"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=sys.stderr.buffer)
    metrics_dir = Path(cfg["metrics_dir"])
    try:
        atomic_json(metrics_dir / f"process-{os.getpid()}.json",
                    {"bridge": process_identity(os.getpid()),
                     "child": process_identity(process.pid) or {"pid": process.pid, "start": None, "zombie": False}})
    except BaseException:
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        raise
    def shutdown():
        while process.poll() is None:
            if (metrics_dir / "shutdown").exists():
                try:
                    process.terminate()
                except ProcessLookupError:
                    pass
                return
            time.sleep(0.02)
    threading.Thread(target=shutdown, daemon=True).start()
    sampler = Sampler(process.pid) if cfg["measure_worker"] else None
    observer = Sampler(os.getpid())
    observer.reset()
    lock = threading.Lock()
    started = not cfg["warm"]
    counts = {"up": 0, "down": 0}
    frames = {"up": {}, "down": {}}
    if sampler:
        sampler.reset(cold=started)
    output = Path(cfg["metrics_dir"]) / f'{"worker" if sampler else "bridge"}-{os.getpid()}.json'

    def meter(direction, kind, length):
        nonlocal started
        with lock:
            if not started and direction == "up" and kind in PLAN_OPS:
                started = True
                if sampler:
                    sampler.reset()
                observer.reset()
            if started:
                counts[direction] += length + 4
                frames[direction][kind] = frames[direction].get(kind, 0) + 1

    def snapshot(completed):
        with lock:
            atomic_json(output, {"completed": completed, "framed_bytes": dict(counts),
                                 "frame_counts": frames, "worker": sampler.result() if sampler else None,
                                 "bridge": observer.result(), "argv": cfg["argv"]})

    errors = []
    def upload():
        try:
            # A daemon waiting in BufferedReader.read can hold Python's
            # stdin buffer lock during interpreter shutdown. FileIO has no
            # such finalizer lock, so the bridge can exit before the master.
            while (body := read_frame(sys.stdin.buffer.raw)) is not None:
                meter("up", frame_kind(body, "up"), len(body))
                process.stdin.write(struct.pack(">I", len(body)) + body)
                process.stdin.flush()
        except (BrokenPipeError, EOFError, OSError) as error:
            errors.append(str(error))
        finally:
            process.stdin.close()

    uplink = threading.Thread(target=upload, daemon=True)
    uplink.start()
    completed = False
    try:
        banner_size = 0
        while True:
            line = process.stdout.readline(65537)
            banner_size += len(line)
            if not line or banner_size > 65536:
                raise RuntimeError("worker did not provide a bounded protocol preamble")
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
            if line == PREAMBLE:
                break
        while (body := read_frame(process.stdout)) is not None:
            kind = frame_kind(body, "down")
            meter("down", kind, len(body))
            if kind == "imported":
                completed = True
                # The final per-worker sample lands before the master sees
                # completion; there is no response/sidecar publication race.
                snapshot(True)
            sys.stdout.buffer.write(struct.pack(">I", len(body)) + body)
            sys.stdout.buffer.flush()
    finally:
        if not completed:
            snapshot(False)
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        atomic_json(metrics_dir / f"exit-{os.getpid()}.json",
                    {"child_pid": process.pid, "child_reaped": True, "returncode": process.returncode})
        if sampler:
            sampler.close()
        observer.close()


def wait_file(path: Path, process=None, seconds=180):
    deadline = time.monotonic() + seconds
    while not path.exists():
        if process is not None and process.poll() is not None:
            raise RuntimeError(f"process exited {process.returncode} before {path.name}")
        if time.monotonic() >= deadline:
            raise TimeoutError(str(path))
        time.sleep(0.005)


def normalized_source_content(text: str) -> bytes:
    """Independent source oracle, not a clipboard parser.

    The formatter envelope terminates each source line with LF; restore drops
    one CR before LF and structural ASCII-blank lines at the body's edges.
    Interior whitespace, Unicode whitespace, markers and header-shaped source
    remain literal content. No FILE recognition or unescaping happens here.
    """
    lines = [line.removesuffix("\r") for line in text.split("\n")]
    while lines and not lines[0].strip(" \t\v\f\r"):
        lines.pop(0)
    while lines and not lines[-1].strip(" \t\v\f\r"):
        lines.pop()
    return "\n".join(lines).encode("utf-8")


def corpus(repo: Path, case: str, scale: float):
    mib = 1024 * 1024
    entries = []
    source = []
    if case == "rust":
        remaining = max(1024, int(mib * scale))
        for path in sorted((repo / "crates").rglob("*.rs")):
            original = path.read_bytes()
            # Keep original line endings and limit by UTF-8 bytes, not chars.
            original.decode("utf-8")
            raw = original[:remaining].decode("utf-8", errors="ignore")
            name = f"source{len(entries):04}.rs"
            entries.append((name, raw))
            source.append({"path": str(path.relative_to(repo)), "sha256": hashlib.sha256(original).hexdigest(),
                           "payload_path": name, "fragment_bytes": len(raw.encode()),
                           "fragment_sha256": hashlib.sha256(raw.encode()).hexdigest()})
            remaining -= len(raw.encode())
            if remaining <= 0:
                break
    elif case == "many":
        entries = [(f"part{i:04}.rs", f"pub const PART_{i}: usize = {i};\n" * 8)
                   for i in range(max(2, int(1200 * scale)))]
    elif case == "large":
        size = max(4096, int((32 * mib - 128 * 1024) * scale))
        entries = [("large.txt", ("a0123456789bcdef\n" * (size // 16 + 1))[:size])]
    elif case == "utf8":
        rng = random.Random(178)
        size = max(4096, int(mib * scale))
        text = "".join(chr(rng.randrange(0x3000, 0xD700)) for _ in range(size // 3))
        entries = [("entropy.txt", text)]
    else:
        raise ValueError(case)
    if not entries:
        raise ValueError(f"empty {case} corpus")
    expected = []
    for name, body in entries:
        content = normalized_source_content(body)
        expected.append({"path": name, "content_bytes": len(content),
                         "content_sha256": hashlib.sha256(content).hexdigest()})
    return [{"path": name, "content": body} for name, body in entries], {
        "case": case, "entries": len(entries), "source_bytes": sum(len(body.encode()) for _, body in entries),
        "expected_entries": expected, "source": source}


def validate_applied_counts(description, result):
    if result.get("createdCount") != description["entries"] or any(result.get(key, 0) for key in
            ("overwrittenCount", "deletedCount", "skippedExistingCount", "plannedSkippedCount")) or result.get("errors") != []:
        raise ValueError("source semantic oracle mismatch: source entries were skipped or changed")


def validate_source_oracle(description, oracle_result, actual_hashes):
    """Reject correlated parser/restore mistakes before timing any sample."""
    expected = description["expected_entries"]
    if len(expected) != description["entries"] or len({entry["path"] for entry in expected}) != len(expected):
        raise ValueError("source manifest has missing or duplicate paths")
    if oracle_result.get("parsed_entries") != expected:
        raise ValueError("source semantic oracle mismatch: parsed paths/count/content differ from declared source")
    validate_applied_counts(description, oracle_result.get("result", {}))
    expected_hashes = {entry["path"]: entry["content_sha256"] for entry in expected}
    if actual_hashes != expected_hashes:
        raise ValueError("source semantic oracle mismatch: restored files differ from normalized original source")
    return expected_hashes


def ssh_argv(host):
    # Per-command control socket disabling makes cold genuinely establish a
    # fresh SSH transport. Compression remains the user's configured value.
    return ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ControlMaster=no", "-o", "ControlPath=none", host]


def remote_call(host, argv, data=None):
    return subprocess.run(ssh_argv(host) + [shlex.join(argv)], input=data, capture_output=True,
                          check=True, timeout=180).stdout


def summary(samples):
    groups = {}
    for sample in samples:
        key = f'{sample["case"]}/v{sample["version"]}/{sample["connection"]}'
        groups.setdefault(key, []).append(sample)
    return {key: {"runs": len(group),
                  "elapsed_median_seconds": statistics.median(s["operation"]["elapsed_seconds"] for s in group),
                  "master_cpu_median_seconds": statistics.median(s["master"]["cpu_seconds"] for s in group),
                  "master_sampled_peak_rss_median_bytes": statistics.median(s["master"]["sampled_peak_rss_bytes"] for s in group),
                  "worker_max_process_sampled_peak_rss_median_bytes": statistics.median(max(p["worker"]["sampled_peak_rss_bytes"] for p in s["workers"]) for s in group),
                  "worker_per_process_peaks_bytes": [[p["worker"]["sampled_peak_rss_bytes"] for p in s["workers"]] for s in group],
                  "worker_cpu_median_seconds": statistics.median(sum(p["worker"]["cpu_seconds"] for p in s["workers"]) for s in group),
                  "up_framed_bytes": [s["wire"]["up"] for s in group],
                  "down_framed_bytes": [s["wire"]["down"] for s in group]}
            for key, group in groups.items()}


def run(args):
    if args.ssh and (not args.remote_snip or not Path(args.remote_snip).is_absolute()):
        raise ValueError("--ssh requires --remote-snip as an absolute isolated worker path")
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    script = Path(__file__).resolve()
    remote_root = None
    report = {"schema": 2, "samples": []}
    try:
        if args.ssh:
            remote_root = remote_call(args.ssh, ["mktemp", "-d", "/tmp/snip-paste-bench-XXXXXXXX"]).decode().strip()
            remote_script = f"{remote_root}/bench_remote_paste.py"
            remote_call(args.ssh, ["python3", "-c", "import pathlib,sys;pathlib.Path(sys.argv[1]).write_bytes(sys.stdin.buffer.read())", remote_script], script.read_bytes())
        source_head = subprocess.check_output(["git", "-C", str(args.repo), "rev-parse", "HEAD"], text=True).strip()
        source_dirty = bool(subprocess.check_output(["git", "-C", str(args.repo), "status", "--porcelain"], text=True).strip())
        worker_info = {"path": str(args.worker.resolve()), "sha256": sha(args.worker), "source_head": args.worker_source_sha or source_head}
        if args.ssh:
            worker_info = {"path": args.remote_snip,
                           "sha256": json.loads(remote_call(args.ssh, ["python3", remote_script, "binary-hash", args.remote_snip])),
                           "source_head": args.worker_source_sha,
                           "source_provenance": "worker source SHA asserted by caller; binary SHA measured remotely"}
        report = {"schema": 2, "measurement": "same-binary PasteTarget plan/apply via framed bridge",
                  "source_oracle": "core formatter plus independent normalized source paths/content hashes; native parsed-entry and local/remote applied-count checks",
                  "rounds": args.rounds, "exploratory": args.rounds < 10 or args.scale != 1,
                  "driver": {"path": str(args.driver.resolve()), "sha256": sha(args.driver)},
                  "worker": worker_info, "source_head": source_head, "source_tree_dirty": source_dirty,
                  "ssh_host": args.ssh, "remote_root": remote_root,
                  "ssh_compression": "inherited, unchanged", "ssh_multiplexing": "disabled for these commands",
                  "rss": "per-process sampled peak (5ms requested interval; lower bound on true peak)",
                  "elapsed": "Rust driver operation; excludes fixture setup, input-file read and result hashing; warm excludes initial OpenWorkspace",
                  "cpu": "own process CPU at readiness/completion barriers; Linux clock ticks / CLK_TCK, Darwin Mach ticks * runtime mach_timebase_info; excludes bridge/ssh CPU (bridge separately retained)",
                  "datasets": [], "samples": [], "process_cleanup": []}
        for case in args.cases.split(","):
            entries, description = corpus(args.repo, case, args.scale)
            report["datasets"].append(description)
            case_dir = out / case
            case_dir.mkdir()
            payload_path = case_dir / "payload.txt"
            manifest_path = case_dir / "source-manifest.json"
            atomic_json(manifest_path, entries)
            del entries
            subprocess.run([str(args.driver.resolve()), "--format-corpus", str(manifest_path), str(payload_path)], check=True, timeout=180)
            payload = payload_path.read_bytes()
            if not 0 < len(payload) <= 32 * 1024 * 1024:
                raise ValueError("formatted corpus must fit the 32 MiB payload budget")
            description.update(payload_bytes=len(payload), payload_sha256=hashlib.sha256(payload).hexdigest(),
                               zlib_ratio_description_only=len(zlib.compress(payload)) / len(payload),
                               formatter="snip_core::format::build_payload")
            del payload
            oracle = case_dir / "oracle"
            oracle.mkdir()
            oracle_control = case_dir / "oracle-control"
            oracle_control.mkdir()
            config = {"payload": str(payload_path), "workspace": str(oracle), "dest": "", "transport": [],
                      "control": str(oracle_control), "warm": False, "oracle": True}
            atomic_json(case_dir / "oracle.json", config)
            subprocess.run([str(args.driver.resolve()), str(case_dir / "oracle.json")], check=True, timeout=180)
            oracle_result = json.loads((oracle_control / "done.json").read_text())
            expected = validate_source_oracle(description, oracle_result, hashes(oracle))
            description["semantic_oracle_verified"] = True
            atomic_json(case_dir / "expected-hashes.json", expected)
            atomic_json(case_dir / "source-oracle.json", {"verified": True, "expected_entries": description["expected_entries"], "parsed_entries": oracle_result["parsed_entries"], "local_result": oracle_result["result"]})
            for round_no in range(args.rounds):
                for connection in ("cold", "warm"):
                    for version in ([6, 7] if round_no % 2 == 0 else [7, 6]):
                        name = f"{case}-r{round_no:02}-{connection}-v{version}"
                        sample_dir = case_dir / name
                        sample_dir.mkdir()
                        dest = sample_dir / "dest"
                        dest.mkdir()
                        metrics_dir = sample_dir / "metrics"
                        metrics_dir.mkdir()
                        local_bridge = {"warm": connection == "warm",
                                        "metrics_dir": str(metrics_dir), "measure_worker": not bool(args.ssh)}
                        if args.ssh:
                            remote_case = f"{remote_root}/{name}"
                            remote_dest = f"{remote_case}/dest"
                            remote_metrics = f"{remote_case}/metrics"
                            remote_call(args.ssh, ["mkdir", "-p", remote_dest, remote_metrics])
                            remote_cfg = {"argv": [args.remote_snip, "serve", "--stdio", "--max-protocol", str(version)],
                                          "warm": connection == "warm", "measure_worker": True, "metrics_dir": remote_metrics}
                            remote_config_path = f"{remote_case}/bridge.json"
                            remote_call(args.ssh, ["python3", "-c", "import pathlib,sys;pathlib.Path(sys.argv[1]).write_bytes(sys.stdin.buffer.read())", remote_config_path], json.dumps(remote_cfg).encode())
                            local_bridge["argv"] = ssh_argv(args.ssh) + [shlex.join(["python3", remote_script, "bridge", remote_config_path])]
                            workspace = remote_dest
                        else:
                            local_bridge["argv"] = [str(args.worker.resolve()), "serve", "--stdio", "--max-protocol", str(version)]
                            workspace = str(dest)
                        atomic_json(sample_dir / "bridge.json", local_bridge)
                        config = {"payload": str(payload_path), "workspace": workspace, "dest": "",
                                  "transport": [sys.executable, str(script), "bridge", str(sample_dir / "bridge.json")],
                                  "control": str(sample_dir), "warm": connection == "warm"}
                        atomic_json(sample_dir / "master.json", config)
                        with (sample_dir / "master.stderr").open("wb") as errors:
                            master = subprocess.Popen([str(args.driver.resolve()), str(sample_dir / "master.json")], stderr=errors)
                            sampler = Sampler(master.pid)
                            try:
                                wait_file(sample_dir / "ready.json", master)
                                sampler.reset()
                                (sample_dir / "go").touch()
                                wait_file(sample_dir / "done.json", master)
                                master_metrics = sampler.result()
                                operation = json.loads((sample_dir / "done.json").read_text())
                                if "error" in operation:
                                    raise RuntimeError(operation["error"])
                                validate_applied_counts(description, operation.get("result", {}))
                                if operation["prepared"] != (version == 7):
                                    raise RuntimeError(f"unexpected prepared path for v{version}: {operation}")
                                local_metrics = [json.loads(p.read_text()) for p in metrics_dir.glob("*.json") if p.name.startswith(("worker-", "bridge-"))]
                                if args.ssh:
                                    worker_metrics = json.loads(remote_call(args.ssh, ["python3", remote_script, "collect", remote_metrics]))
                                    actual = json.loads(remote_call(args.ssh, ["python3", remote_script, "hash", workspace]))
                                else:
                                    worker_metrics = local_metrics
                                    actual = hashes(dest)
                                if actual != expected:
                                    raise RuntimeError(f"destination hash mismatch: {name}")
                                if not worker_metrics or not all(p["completed"] and p["worker"]["cpu_seconds"] is not None for p in worker_metrics):
                                    raise RuntimeError("missing completed per-process worker telemetry")
                                if master_metrics["cpu_seconds"] is None:
                                    raise RuntimeError("missing master CPU sample")
                                sample = {"case":case,"round":round_no,"version":version,"connection":connection,
                                          "operation":operation,"master":master_metrics,"workers":worker_metrics,
                                          "bridges":local_metrics,"wire":{d:sum(p["framed_bytes"][d] for p in local_metrics) for d in ("up","down")},
                                          "hashes":actual,"hashes_match":True}
                                report["samples"].append(sample)
                                atomic_json(sample_dir / "result.json", sample)
                                report["summary"] = summary(report["samples"])
                                atomic_json(out / "results.json", report)
                                print(f'{name}: {operation["elapsed_seconds"]:.4f}s up={sample["wire"]["up"]} down={sample["wire"]["down"]}', flush=True)
                            finally:
                                process_cleanup = {"sample": name}
                                try:
                                    if args.ssh:
                                        try:
                                            process_cleanup["remote"] = json.loads(remote_call(args.ssh, ["python3", remote_script, "finish", remote_metrics]))
                                        except Exception as error:
                                            process_cleanup["remote"] = {"completed": False, "error": str(error)}
                                    process_cleanup["local"] = finish_metrics(metrics_dir)
                                    process_cleanup["completed"] = all(part["completed"] for key, part in process_cleanup.items() if key in ("local", "remote"))
                                except Exception as error:
                                    process_cleanup.update(completed=False, error=str(error))
                                report["process_cleanup"].append(process_cleanup)
                                (sample_dir / "release").touch()
                                try:
                                    master.wait(timeout=10)
                                except subprocess.TimeoutExpired:
                                    master.kill()
                                    master.wait(timeout=5)
                                sampler.close()
                        if master.returncode:
                            raise RuntimeError(f"master exited {master.returncode}: {sample_dir}")
        print(out / "results.json")

    finally:
        if sys.exc_info()[1] is not None:
            report["failure"] = str(sys.exc_info()[1])
        processes_ended = all(item["completed"] for item in report.get("process_cleanup", []))
        cleanup = {"remote_root": remote_root, "processes_confirmed_ended": processes_ended, "completed": remote_root is None and processes_ended}
        if remote_root:
            try:
                if not processes_ended:
                    raise RuntimeError("retaining remote artifacts because owned process exit was not confirmed")
                if not re.fullmatch(r"/tmp/snip-paste-bench-[A-Za-z0-9]{8}", remote_root):
                    raise RuntimeError("refusing cleanup of an unrecognized mktemp result")
                remote_call(args.ssh, ["rm", "-rf", "--", remote_root])
                cleanup["completed"] = True
            except Exception as error:
                cleanup["error"] = str(error)
        report["cleanup"] = cleanup
        atomic_json(out / "results.json", report)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "bridge":
        bridge(Path(sys.argv[2])); return
    if len(sys.argv) > 1 and sys.argv[1] == "hash":
        print(json.dumps(hashes(Path(sys.argv[2])))); return
    if len(sys.argv) > 1 and sys.argv[1] == "finish":
        print(json.dumps(finish_metrics(Path(sys.argv[2])))); return
    if len(sys.argv) > 1 and sys.argv[1] == "binary-hash":
        print(json.dumps(sha(Path(sys.argv[2])))); return
    if len(sys.argv) > 1 and sys.argv[1] == "collect":
        print(json.dumps([json.loads(p.read_text()) for p in Path(sys.argv[2]).glob("worker-*.json")])); return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True, help="local snip binary (also identified in local runs)")
    parser.add_argument("--remote-snip", help="absolute isolated remote snip binary; never overwrites an installed binary")
    parser.add_argument("--ssh")
    parser.add_argument("--worker-source-sha", help="exact source commit of the supplied worker (required for SSH)")
    parser.add_argument("--out", type=Path, required=True, help="new output directory")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--cases", default="rust,many,large,utf8")
    parser.add_argument("--scale", type=float, default=1.0, help="smoke only; non-1 marks report exploratory")
    args = parser.parse_args()
    if args.ssh and (not args.worker_source_sha or not re.fullmatch(r"[0-9a-f]{40}", args.worker_source_sha)):
        parser.error("--ssh requires --worker-source-sha as the exact 40-character source commit")
    if args.rounds < 1 or not 0 < args.scale <= 1:
        parser.error("rounds must be positive and 0 < scale <= 1")
    run(args)


if __name__ == "__main__":
    main()
