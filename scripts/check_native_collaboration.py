#!/usr/bin/env python3
"""Drive two private native UI sessions through the collaboration manifest.

This is a historical UI pilot when the operator passes a prebuilt binary.
Exit 0 requires all 18 Linux functional steps with complete evidence.
Release acceptance also requires graph edges, all platforms, a release
binary, and the separate memory gate. A subset, missing control, timeout,
or cleanup error is nonzero.

The driver does not own Xvfb, D-Bus, or the clipboard. Those stay in
NativeSession from bench_native_memory.py (sibling scripts/ on final
integration, or PYTHONPATH here). It does not call the sync CLI or core.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import traceback
import uuid
import warnings
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

SCRIPTS_DIR = os.path.abspath(os.path.dirname(__file__))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from collaboration_fixture import (  # noqa: E402
    CompareError,
    EMPTY_TREE,
    VerificationError,
    compare_step,
    parse_diff_tree_z,
    snapshot,
    verify,
)

PILOT_IDS = (
    "file-a-to-b-pair01",
    "file-b-to-a-pair09",
    "commit-a-to-b-pair02",
    "commit-b-to-a-unmerged-pair03",
)
REQUIRED_STEP_IDS = (
    "file-a-to-b-pair01",
    "file-b-to-a-pair09",
    "file-explicit-valid-root-pair01-to-pair15b",
    "commit-a-to-b-pair02",
    "commit-b-to-a-unmerged-pair03",
    "commit-merge-first-parent-pair04",
    "commit-root-pair05",
    "commit-octopus-first-parent-pair01",
    "commit-rename-delete-pair08",
    "neg-cross-repo-commits",
    "neg-noncontiguous-tips",
    "neg-mapping-collision",
    "neg-mapping-ambiguous-basename",
    "neg-mapping-missing-destination",
    "neg-stale-source",
    "neg-stale-target",
    "neg-overwrite-unauthorized",
    "neg-cancel",
)
EXPECTED_SUMMARY = {
    "repoCount": 30,
    "originCount": 15,
    "commitCount": 428,
    "refCount": 366,
    "positiveStepCount": 9,
    "negativeStepCount": 9,
}
REQUIRED_TOOLS = ("Xvfb", "xdotool", "xclip", "convert", "xwd")
EVIDENCE_KEYS = (
    "id",
    "status",
    "actions",
    "probeGenerations",
    "source",
    "clipboard",
    "screenshots",
    "hashes",
    "snapshots",
    "compare",
    "failures",
    "cleanup",
)
GIT_READ_COMMANDS = frozenset(
    {"rev-parse", "rev-list", "log", "status", "cat-file", "diff", "diff-tree", "show", "ls-files", "ls-tree"}
)
PASTE_MAP_CANDIDATE_RE = re.compile(
    r"\[APP:PASTE_MAP_CANDIDATE:\s+prefix=(?P<prefix>.+?)\s+idx=(?P<idx>\d+)\s+path=(?P<path>.+?)\]"
)
PASTE_MAPPED_RE = re.compile(
    r"\[APP:PASTE_MAPPED:\s+prefix=(?P<prefix>.+?)(?:\s+keep=(?P<keep>\S+))?\s+dest=(?P<dest>.+?)\s+items=(?P<items>\d+)\]"
)
PASTE_PREVIEW_RE = re.compile(
    r"\[APP:PASTE_PREVIEW:\s+items=(?P<items>\d+)\s+dest=(?P<dest>.+?)\s+mapping=(?P<mapping>true|false)\]"
)
HELP = """\
Drive the native workbench through collaboration manifest steps.

Exit 0 means all 18 Linux functional steps passed on the real UI with
complete evidence. Release acceptance also needs painted graph edges,
every required platform, a release binary, and the separate memory gate.
--help does not run the GUI and is not a pass. A pilot subset, blocked
control, timeout, clipboard mismatch, or cleanup error exits nonzero.
"""


class DriverError(Exception):
    """Fail-closed driver error. Never a successful acceptance."""


class MissingControl(DriverError):
    def __init__(self, control_id: str, detail: str = ""):
        self.control_id = control_id
        message = f"missing control {control_id}"
        if detail:
            message = f"{message}: {detail}"
        super().__init__(message)


class OperationTimeout(DriverError):
    pass


class UiRefusal(DriverError):
    def __init__(self, line: str):
        self.line = line
        super().__init__(line)


class ClipboardMismatch(DriverError):
    pass


def install_resource_warning_filter() -> None:
    warnings.simplefilter("error", ResourceWarning)


def parse_timeout(value: object) -> float:
    if isinstance(value, bool) or value is None:
        raise DriverError("per-operation timeout must be a finite positive number")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise DriverError("per-operation timeout must be a finite positive number") from exc
    if not math.isfinite(number) or number <= 0:
        raise DriverError("per-operation timeout must be a finite positive number")
    return number


def require_absolute(path: str, what: str) -> Path:
    if not path or not os.path.isabs(path):
        raise DriverError(f"{what} must be an absolute path")
    return Path(path)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def check_sha256(value: str, what: str) -> str:
    if len(value) != 64 or any(ch not in "0123456789abcdef" for ch in value.lower()):
        raise DriverError(f"{what} must be a SHA-256 hex digest")
    return value.lower()


def check_binary(path: Path, expected_sha: str) -> dict[str, str]:
    if not path.is_file():
        raise DriverError(f"binary is missing or not a regular file: {path}")
    actual = sha256_file(path)
    if actual != check_sha256(expected_sha, "binary sha"):
        raise DriverError(f"binary sha mismatch: actual {actual}")
    return {"path": str(path), "sha256": actual}


def check_dataset_hash(manifest: Mapping[str, Any], expected_hash: str) -> None:
    expected = check_sha256(expected_hash, "dataset hash")
    actual = str(manifest.get("datasetHash") or "")
    if actual.lower() != expected:
        raise DriverError(f"fixture datasetHash mismatch: actual {actual or 'missing'}")


def require_manifest_contract(manifest: Mapping[str, Any]) -> list[str]:
    summary = manifest.get("summary") or {}
    for key, want in EXPECTED_SUMMARY.items():
        if summary.get(key) != want:
            raise DriverError(f"fixture summary {key}={summary.get(key)!r}, contract wants {want}")
    steps = manifest.get("steps") or []
    ids = [str(step.get("id")) for step in steps]
    if len(ids) != 18 or len(set(ids)) != 18:
        raise DriverError(f"fixture does not have 18 independent steps ({len(ids)})")
    if set(ids) != set(REQUIRED_STEP_IDS):
        missing = sorted(set(REQUIRED_STEP_IDS) - set(ids))
        extra = sorted(set(ids) - set(REQUIRED_STEP_IDS))
        raise DriverError(f"fixture step IDs do not match required 18 manifest steps (missing: {missing}, extra: {extra})")
    return ids


def require_tools(tools: Sequence[str] = REQUIRED_TOOLS, lookup: Callable[[str], str | None] | None = None) -> None:
    find = lookup or shutil.which
    missing = [name for name in tools if not find(name)]
    if missing:
        raise DriverError(
            "unsupported display/tools; refusing to install packages. Missing: " + ", ".join(missing)
        )


def reject_output(path: Path, fixture: Path) -> None:
    if path.is_symlink():
        raise DriverError(f"output path is a symlink: {path}")
    fixture_real = os.path.realpath(fixture)
    output_real = os.path.realpath(path)
    if output_real == fixture_real or output_real.startswith(fixture_real + os.sep):
        raise DriverError("output path overlaps the fixture")
    if fixture_real.startswith(output_real + os.sep):
        raise DriverError("fixture path is inside the output directory")
    if path.exists():
        if not path.is_dir():
            raise DriverError(f"output path exists and is not a directory: {path}")
        if any(path.iterdir()):
            raise DriverError(f"output directory is not empty: {path}")


def accept_baseline(fixture: Path, expected_hash: str, verifier: Callable[[str], Mapping[str, Any]]) -> dict[str, Any]:
    manifest_path = fixture / "manifest.json"
    if not manifest_path.is_file():
        raise DriverError(f"fixture manifest is missing: {manifest_path}")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise DriverError(f"fixture manifest is unreadable: {exc}") from exc
    check_dataset_hash(manifest, expected_hash)
    try:
        result = verifier(str(fixture))
    except (VerificationError, OSError, DriverError) as exc:
        raise DriverError(f"fixture verify failed: {exc}") from exc
    if str(result.get("datasetHash") or "").lower() != expected_hash.lower():
        raise DriverError("verifier reported a different datasetHash")
    require_manifest_contract(manifest)
    return manifest


def ui_repo_names(repos: Sequence[Mapping[str, Any]]) -> dict[str, str]:
    """The native workbench uses workspace-relative labels for duplicate basenames."""
    counts = Counter(repo["basename"] for repo in repos)
    return {
        str(repo["repoId"]): str(Path(repo["relativePath"]).relative_to(Path(repo["relativePath"]).parts[0]))
        if counts[repo["basename"]] > 1 else str(repo["basename"])
        for repo in repos
    }


def control_basename(control_id: str) -> str:
    """pick-repo:ledger and pick-repo:4:billing both name the basename, never the root."""
    prefix = "pick-repo:"
    if not control_id.startswith(prefix):
        raise DriverError(f"not a repo pick: {control_id}")
    rest = control_id[len(prefix) :]
    head, sep, tail = rest.partition(":")
    if sep and head.isdigit():
        return tail
    return rest


def indexed_pick(control_id: str) -> bool:
    rest = control_id[len("pick-repo:") :]
    head, sep, _tail = rest.partition(":")
    return bool(sep and head.isdigit())


def selecting_fields(line: str) -> tuple[str, str]:
    name = line.split("(", 1)[1].split(")", 1)[0]
    root = line.split("root=", 1)[1].rsplit("]", 1)[0].strip()
    return name, root


def change_control(kind: str, path: str) -> tuple[str, str]:
    if kind == "fixed-oid":
        raise MissingControl(
            f"fixed-oid:{path}",
            "historical tree selection uses rev-chk:<commit>:<path>, not change-chk",
        )
    if kind == "delete":
        raise DriverError(f"cannot treat operation kind 'delete' as source for {path}")
    source = {"working": "unstaged", "index": "staged", "unstaged": "unstaged", "staged": "staged"}.get(kind)
    if source is None:
        raise MissingControl(f"{kind}:{path}", f"unsupported source kind {kind!r}")
    return f"change-row:{source}:{path}", f"change-chk:{source}:{path}"


def fixed_oid_controls(commit: str, path: str) -> tuple[str, str]:
    return f"rev-row:{path}", f"rev-chk:{commit}:{path}"


def fixed_oid_blocker(path: str, rev: str, oid: str) -> str:
    return (
        f"no rendered checkbox exports fixed OID {rev}:{path} blob {oid}. "
        "commit-file:{path} and rev-row:{path} only preview. "
        "btn-copy of a commit selection is COPY_REFUSED commit_readonly. "
        "a Project tree selection would copy worktree bytes and is not a substitute"
    )


def resolve_operation_source(op: Mapping[str, Any]) -> tuple[str, str]:
    """Resolve UI source ('unstaged' or 'staged') and path for an operation.

    In the collaboration manifest, deletions have op: 'delete' while source.kind
    is already 'working' or 'index' ('working' -> 'unstaged', 'index' -> 'staged').
    Validates required deletion provenance (status='D', inWorktree=False, diffAgainst,
    and blob OIDs). Treating operation kind 'delete' as a source kind fails.
    """
    source = op.get("source") or {}
    path = source.get("path") or op.get("sourcePath")
    if not path:
        raise DriverError("operation missing source path")

    is_delete = (
        op.get("op") == "delete"
        or op.get("kind") == "delete"
        or source.get("status") == "D"
    )

    if is_delete:
        source_kind = source.get("kind") or op.get("sourceKind")
        if source_kind == "delete":
            raise DriverError(
                f"deletion operation for {path} treats 'delete' as source kind without required working/index provenance"
            )
        if source_kind not in ("working", "index"):
            raise DriverError(f"ambiguous or invalid deletion source kind {source_kind!r} for {path}")

        status = source.get("status")
        if status is not None and status != "D":
            raise DriverError(f"deletion provenance status for {path} must be 'D', got {status!r}")
        if source.get("inWorktree") is True:
            raise DriverError(f"deletion provenance for {path} cannot have inWorktree=True")

        diff_against = source.get("diffAgainst")
        in_index = source.get("inIndex")
        if source_kind == "working":
            if diff_against is not None and diff_against != "index":
                raise DriverError(f"working deletion for {path} must diffAgainst 'index', got {diff_against!r}")
            if in_index is not None and in_index is not True:
                raise DriverError(f"working deletion for {path} must have inIndex=True")
            if not source.get("oid"):
                raise DriverError(f"working deletion for {path} missing blob oid")
            return "unstaged", str(path)
        elif source_kind == "index":
            if diff_against is not None and diff_against != "HEAD":
                raise DriverError(f"index deletion for {path} must diffAgainst 'HEAD', got {diff_against!r}")
            if in_index is not None and in_index is not False:
                raise DriverError(f"index deletion for {path} must have inIndex=False")
            if not (source.get("headOid") or source.get("oid")):
                raise DriverError(f"index deletion for {path} missing HEAD blob oid")
            return "staged", str(path)

    kind = source.get("kind") or op.get("kind") or op.get("sourceKind")
    if kind == "fixed-oid":
        return "fixed-oid", str(path)
    if kind == "delete":
        raise DriverError(f"operation kind 'delete' cannot be used as source for {path}")
    source_mapped = {"working": "unstaged", "index": "staged", "unstaged": "unstaged", "staged": "staged"}.get(kind)
    if source_mapped is None:
        raise MissingControl(f"{kind}:{path}", f"unsupported source kind {kind!r}")
    return source_mapped, str(path)


def bridge_clipboard(payload: bytes, readback: bytes) -> dict[str, Any]:
    if not isinstance(payload, (bytes, bytearray)) or not isinstance(readback, (bytes, bytearray)):
        raise ClipboardMismatch("clipboard bridge requires raw bytes")
    payload = bytes(payload)
    readback = bytes(readback)
    if payload != readback:
        raise ClipboardMismatch(
            f"destination readback sha {sha256_bytes(readback)} != source sha {sha256_bytes(payload)}"
        )
    if not payload:
        raise ClipboardMismatch("clipboard bridge was empty")
    return {"sha256": sha256_bytes(payload), "bytes": len(payload), "equal": True}


def transfer_os_clipboard(source: Any, dest: Any) -> dict[str, Any]:
    payload = source.read_clipboard()
    if not isinstance(payload, (bytes, bytearray)):
        raise ClipboardMismatch("source read_clipboard did not return bytes")
    dest.set_clipboard(bytes(payload))
    return bridge_clipboard(bytes(payload), dest.read_clipboard())


def require_bounds(bounds: Mapping[str, Any], control_id: str) -> tuple[int, int, int, int]:
    if control_id not in bounds:
        raise MissingControl(control_id)
    box = bounds[control_id]
    if len(box) != 4 or int(box[2]) <= 0 or int(box[3]) <= 0:
        raise MissingControl(control_id, f"empty bounds {box}")
    return int(box[0]), int(box[1]), int(box[2]), int(box[3])


def counts_as_pass(status: str) -> bool:
    return status == "passed"


def classify_exception(exc: BaseException) -> str:
    if isinstance(exc, MissingControl):
        return "blocked"
    if isinstance(exc, OperationTimeout):
        return "timeout"
    if isinstance(exc, UiRefusal):
        return "refused"
    return "failed"


def graph_from_lines(lines: Sequence[str]) -> dict[str, Any]:
    """Count lines are not painted edges. The pending graph schema is not a control."""
    return {
        "paintedEdgesProven": False,
        "loaded": [line for line in lines if "GRAPH_LOADED:" in line or "E2E_LOG:" in line],
        "edgeProbes": [line for line in lines if "GRAPH_EDGE" in line or "E2E_GRAPH_EDGE" in line],
        "gap": (
            "D5: GRAPH_LOADED / E2E_LOG report counts and the first row only. "
            "Direct git parent comparison is a fixture oracle, not painted edges. "
            "graph-observation-schema is a pending proposal, not an available control."
        ),
    }


def evidence_omissions(step: Mapping[str, Any]) -> list[str]:
    missing = [key for key in EVIDENCE_KEYS if key not in step]
    if step.get("status") == "passed":
        for key in ("actions", "clipboard", "screenshots", "snapshots", "compare", "cleanup"):
            value = step.get(key)
            if value in (None, "", [], {}):
                missing.append(f"empty:{key}")
        cleanup = step.get("cleanup") or {}
        if cleanup.get("errors") or cleanup.get("survivors") or cleanup.get("appExit", 0) != 0:
            missing.append("cleanup-not-clean")
    return missing


def apply_cleanup(
    step: dict[str, Any],
    errors: Sequence[str],
    survivors: Sequence[Mapping[str, Any]],
    app_exit: int | None = 0,
    forced: bool = False,
    apps: Sequence[Mapping[str, Any]] | None = None,
    drained: bool = True,
) -> dict[str, Any]:
    all_errors = list(errors)
    if app_exit is None:
        all_errors.append("app process exit code was null (timed out or forced kill)")
    elif app_exit != 0:
        all_errors.append(f"app process exited with code {app_exit}")
    if forced:
        all_errors.append("app process required forced termination")
    if not drained:
        all_errors.append("cleanup missing drained confirmation")
    step["cleanup"] = {
        "errors": all_errors,
        "survivors": list(survivors),
        "appExit": app_exit,
        "forced": forced,
        "drained": drained,
        "apps": list(apps) if apps is not None else [],
    }
    if all_errors or survivors or app_exit != 0 or forced or not drained:
        step.setdefault("failures", [])
        step["failures"].append("cleanup failed")
        if step.get("status") == "passed":
            step["status"] = "failed"
    return step


def unmet_release_requirements(report: Mapping[str, Any]) -> list[str]:
    problems: list[str] = []
    graph = report.get("graph") or {}
    if graph.get("paintedEdgesProven") is not True:
        problems.append("painted graph edges are not proven")
    platform = report.get("platform") or {}
    if platform.get("complete") is not True:
        problems.append("platform coverage is incomplete")
    if report.get("historicalBinary") is True:
        problems.append("binary is a historical pilot, not the final revision")
    memory = report.get("memoryGate") or {}
    if not memory.get("executed"):
        problems.append("independent memory gate was not executed")
    return problems


def functional_problems(report: Mapping[str, Any],
                        required_ids: Sequence[str] = REQUIRED_STEP_IDS) -> list[str]:
    """`required_ids` narrows the step set for one shard of a sharded run;
    the merged report is checked again against every manifest step."""
    problems: list[str] = []
    if report.get("synthetic") or report.get("syntheticCompleteness"):
        problems.append("synthetic result is not native acceptance")
    if report.get("memoryAbsenceClaimed") is True:
        problems.append("memory leak absence was claimed")
    if report.get("releaseClaimed") or report.get("ciGreen") or report.get("merged"):
        problems.append("release, CI, or merge was claimed")
    if not report.get("binary"):
        problems.append("missing binary")
    binary_sha = report.get("binarySha256")
    if not binary_sha or len(str(binary_sha)) != 64:
        problems.append("missing or invalid binarySha256")
    dataset_hash = report.get("datasetHash")
    if not dataset_hash or len(str(dataset_hash)) != 64:
        problems.append("missing or invalid datasetHash")

    steps = list(report.get("steps") or [])
    step_ids = [str(step.get("id")) for step in steps if step.get("id")]
    if set(step_ids) != set(required_ids) or len(step_ids) != len(required_ids):
        missing = sorted(set(required_ids) - set(step_ids))
        extra = sorted(set(step_ids) - set(required_ids))
        problems.append(
            f"step set does not match required {len(required_ids)} manifest step IDs (missing: {missing}, extra: {extra})"
        )

    for step in steps:
        step_id = str(step.get("id"))
        status = str(step.get("status"))
        if not counts_as_pass(status):
            problems.append(f"{step_id}: {status}")
        failures = step.get("failures")
        expected_kind = "negative" if step_id.startswith("neg-") else "positive-commit" if step_id.startswith("commit-") else "positive-file"
        if step.get("kind") != expected_kind:
            problems.append(f"{step_id}: kind does not match required step ID ({step.get('kind')} != {expected_kind})")
        if failures:
            problems.append(f"{step_id}: non-empty failures {failures}")
        omissions = evidence_omissions(step)
        if omissions and counts_as_pass(status):
            problems.append(f"{step_id}: evidence missing {omissions}")

        # Real clipboard proof validation
        cb = step.get("clipboard")
        if isinstance(cb, dict):
            kind = step.get("kind")
            if kind == "negative":
                sentinel_sha = cb.get("sentinelSha256")
                source_sha = cb.get("sourceSha256")
                refused = cb.get("refused") or cb.get("refusal")
                has_valid_sentinel = bool(
                    sentinel_sha
                    and len(str(sentinel_sha)) == 64
                    and cb.get("clipboardMatchesSentinel") is True
                )
                has_valid_source = bool(source_sha and len(str(source_sha)) == 64)
                has_refusal = bool(refused and len(str(refused).strip()) > 0)
                outcome = str(cb.get("apply") or cb.get("cancel") or "")
                has_expected_outcome = (
                    (step_id == "neg-stale-target" and "[APP:PASTE_STALE_DETECTED:" in outcome)
                    or (step_id == "neg-cancel" and "[APP:PASTE_CANCELLED]" in outcome)
                    or (step_id == "neg-overwrite-unauthorized" and "[APP:PASTE_DONE:" in outcome and "overwritten=0" in outcome)
                )
                has_proof = (
                    has_valid_sentinel
                    or (has_valid_source and has_refusal)
                    or (has_valid_source and has_expected_outcome)
                    or (has_valid_source and cb.get("crossRepoPrevented") is True and cb.get("exactPayloadOracle") is True)
                )
                if step_id in ("neg-stale-target", "neg-cancel", "neg-overwrite-unauthorized"):
                    has_proof = has_valid_source and has_expected_outcome
                if not has_proof:
                    problems.append(f"{step_id}: negative step clipboard lacks proof")
                if step_id == "neg-mapping-missing-destination" and not valid_whitelist_prevention(cb):
                    problems.append(f"{step_id}: incomplete canonical destination whitelist prevention evidence")
            else:
                src = cb.get("source") if isinstance(cb.get("source"), dict) else {}
                rb = cb.get("readback") if isinstance(cb.get("readback"), dict) else {}
                src_sha = src.get("sha256")
                rb_sha = rb.get("sha256")
                if not src_sha or len(str(src_sha)) != 64 or not rb_sha or len(str(rb_sha)) != 64:
                    problems.append(f"{step_id}: positive step clipboard missing 64-char sha256")
                elif src_sha != rb_sha:
                    problems.append(f"{step_id}: positive clipboard readback differs from source")

        # Cleanup validation
        cleanup = step.get("cleanup") or {}
        if (
            cleanup.get("errors") != []
            or cleanup.get("survivors") != []
            or type(cleanup.get("appExit")) is not int
            or cleanup.get("appExit") != 0
            or cleanup.get("forced") is not False
            or cleanup.get("drained") is not True
        ):
            problems.append(f"{step_id}: cleanup failed {cleanup}")
        apps = cleanup.get("apps")
        if not isinstance(apps, list) or len(apps) != 2:
            problems.append(
                f"{step_id}: cleanup must record exactly two app processes (got {len(apps) if isinstance(apps, list) else type(apps).__name__})"
            )
        else:
            seen_pids = set()
            for i, app_rec in enumerate(apps):
                if not isinstance(app_rec, dict):
                    problems.append(f"{step_id}: cleanup app[{i}] is not a dict")
                    continue
                ident = app_rec.get("app")
                if not isinstance(ident, dict):
                    problems.append(f"{step_id}: cleanup app[{i}] missing app identity dict")
                    continue
                pid = ident.get("pid")
                starttime = ident.get("starttime")
                if not valid_app_identity(ident):
                    problems.append(f"{step_id}: cleanup app[{i}] has invalid pid or starttime: {ident}")
                else:
                    seen_pids.add((pid, starttime))
                if type(app_rec.get("appExit")) is not int or app_rec.get("appExit") != 0:
                    problems.append(f"{step_id}: cleanup app[{i}] appExit {app_rec.get('appExit')} != 0")
                if app_rec.get("forced") is not False:
                    problems.append(f"{step_id}: cleanup app[{i}] forced is not False")
                if app_rec.get("drained") is not True:
                    problems.append(f"{step_id}: cleanup app[{i}] drained is not True")
                if app_rec.get("errors") != []:
                    problems.append(f"{step_id}: cleanup app[{i}] has errors: {app_rec.get('errors')}")
                if app_rec.get("survivors") != []:
                    problems.append(f"{step_id}: cleanup app[{i}] has survivors: {app_rec.get('survivors')}")
            if len(seen_pids) != 2:
                problems.append(f"{step_id}: cleanup apps do not have two distinct identities ({seen_pids})")

        # Oracle comparison validation for negative steps
        cmp = step.get("compare")
        if isinstance(cmp, dict) and step.get("kind") == "negative":
            applied = cmp.get("applied")
            if applied not in ("equals-baseline", "equals-pre-action-snapshot"):
                problems.append(f"{step_id}: negative step compare lacks no-write verification ({applied})")
            if step_id == "neg-mapping-missing-destination" and applied != "equals-baseline":
                problems.append(f"{step_id}: whitelist prevention lacks unchanged full baseline")

    if report.get("acceptanceComplete") is True:
        unmet = unmet_release_requirements(report)
        if unmet:
            problems.append("acceptanceComplete was set while release requirements remain")
    return problems


def valid_whitelist_prevention(cb: Mapping[str, Any]) -> bool:
    candidates = cb.get("candidateCanonicalRoots")
    expected = cb.get("workspaceCanonicalRoots")
    repo_ids = cb.get("candidateRepoIds")
    return bool(
        cb.get("prevention") == "canonical-destination-whitelist"
        and cb.get("originalMappedIdSubmitted") is False
        and cb.get("automaticMappingObserved") is False
        and cb.get("disabledApplyClicked") is True
        and cb.get("refusal") == "[APP:PASTE_ERR: mapping_required]"
        and cb.get("clipboardMatchesSource") is True
        and cb.get("readbackSha256") == cb.get("sourceSha256")
        and isinstance(candidates, list) and isinstance(expected, list)
        and len(candidates) == len(expected) == 15
        and all(isinstance(path, str) and os.path.isabs(path) for path in candidates + expected)
        and len(set(candidates)) == 15 and sorted(candidates) == sorted(expected)
        and isinstance(repo_ids, list) and len(repo_ids) == 15
        and all(isinstance(rid, str) and rid.startswith("b-") for rid in repo_ids)
        and len(set(repo_ids)) == 15
        and cb.get("missingDestRepoId") == "b-does-not-exist"
        and cb["missingDestRepoId"] not in repo_ids
        and all(cb["missingDestRepoId"] not in Path(path).parts for path in candidates)
    )


def release_problems(report: Mapping[str, Any]) -> list[str]:
    return list(functional_problems(report)) + unmet_release_requirements(report)


def acceptance_problems(report: Mapping[str, Any], scope: str = "release") -> list[str]:
    if scope == "functional":
        return functional_problems(report)
    return release_problems(report)


def functional_exit_code(report: Mapping[str, Any]) -> int:
    return 0 if not functional_problems(report) else 1


def release_exit_code(report: Mapping[str, Any]) -> int:
    return 0 if not release_problems(report) else 1


def acceptance_exit_code(report: Mapping[str, Any], scope: str = "functional") -> int:
    if scope == "release":
        return release_exit_code(report)
    return functional_exit_code(report)


def blank_step(step_id: str) -> dict[str, Any]:
    return {
        "id": step_id,
        "status": "not-run",
        "actions": [],
        "probeGenerations": [],
        "source": {},
        "clipboard": {},
        "screenshots": {},
        "hashes": {},
        "snapshots": {},
        "compare": {},
        "failures": [],
        "cleanup": {"errors": [], "survivors": []},
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="check_native_collaboration", description=HELP)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--binary-sha", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--dataset-hash", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--timeout", required=True, help="Finite seconds for each UI/git operation")
    parser.add_argument("--helper-receipt")
    parser.add_argument("--phase", choices=("pilot", "all"), default="pilot")
    parser.add_argument("--steps", help="Comma-separated manifest step ids. Overrides --phase")
    return parser


def selected_ids(manifest: Mapping[str, Any], phase: str, steps: str | None) -> list[str]:
    ids = [str(step["id"]) for step in manifest["steps"]]
    if steps:
        chosen = [part.strip() for part in steps.split(",") if part.strip()]
        unknown = [step_id for step_id in chosen if step_id not in ids]
        if not chosen or unknown:
            raise DriverError(f"unknown steps: {unknown or chosen}")
        return chosen
    if phase == "pilot":
        return list(PILOT_IDS)
    return ids


def git_read(repo: Path, args: Sequence[str], timeout: float) -> str:
    return git_read_bytes(repo, args, timeout).decode("utf-8", "replace")


def git_read_bytes(repo: Path, args: Sequence[str], timeout: float) -> bytes:
    if not args or args[0] not in GIT_READ_COMMANDS:
        raise DriverError(f"refusing git command {args[:1]}")
    env = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
        env.pop(key, None)
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_TERMINAL_PROMPT"] = "0"
    try:
        result = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            timeout=timeout,
            env=env,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise OperationTimeout(f"git {' '.join(args)} timed out") from exc
    if result.returncode != 0:
        err = result.stderr.decode("utf-8", "replace").strip()[:300]
        raise DriverError(f"git {' '.join(args)} failed: {err}")
    return result.stdout


def load_native() -> Any:
    try:
        import bench_native_memory as native
    except ImportError as exc:
        raise DriverError(
            "NativeSession helper is not importable. Final integration places "
            "bench_native_memory.py next to this script; this worktree supplies it "
            "with PYTHONPATH. Refusing to guess a temporary path."
        ) from exc
    for name in ("NativeSession", "parse_bounds", "require_control", "scroll_into_view", "set_clipboard"):
        if name == "set_clipboard":
            if not hasattr(native.NativeSession, "set_clipboard"):
                raise DriverError("NativeSession.set_clipboard is missing")
            continue
        if not hasattr(native, name):
            raise DriverError(f"helper is missing {name}")
    return native


def helper_hashes(native: Any, receipt: Path | None) -> dict[str, Any]:
    files = {
        "bench_native_memory.py": Path(native.__file__).resolve(),
    }
    scripts = files["bench_native_memory.py"].parent
    path = scripts / "memory_harness.py"
    if not path.is_file():
        raise DriverError("helper sibling missing: memory_harness.py")
    files["memory_harness.py"] = path
    hashed = {name: sha256_file(path) for name, path in files.items()}
    checked = None
    if receipt is not None:
        try:
            payload = json.loads(receipt.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise DriverError(f"helper receipt is unreadable: {exc}") from exc
        mismatches = []
        for name, digest in hashed.items():
            recorded = ((payload.get("files") or {}).get(name) or {}).get("sha256")
            if recorded != digest:
                mismatches.append(f"{name} actual {digest} receipt {recorded}")
        if mismatches:
            raise DriverError("helper hash mismatch: " + "; ".join(mismatches))
        checked = str(receipt)
    return {"files": hashed, "receipt": checked}


def write_json(path: Path, payload: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def copy_fixture(source: Path, dest: Path) -> None:
    if dest.exists() or dest.is_symlink():
        raise DriverError(f"refusing to replace existing path {dest}")
    shutil.copytree(source, dest, symlinks=True, copy_function=shutil.copy2)


def repo_by_id(manifest: Mapping[str, Any], repo_id: str) -> dict[str, Any]:
    for repo in manifest["repos"]:
        if repo["repoId"] == repo_id:
            return repo
    raise DriverError(f"manifest has no repo {repo_id}")


def machine_of(repo: Mapping[str, Any]) -> str:
    relative = str(repo["relativePath"])
    if relative.startswith("machine-a/"):
        return "a"
    if relative.startswith("machine-b/"):
        return "b"
    raise DriverError(f"repo is not on machine A or B: {relative}")


def snapshot_file(fixture: Path, dest: Path) -> dict[str, Any]:
    payload = snapshot(str(fixture))
    write_json(dest, payload)
    return payload


def snapshot_digest(path: Path) -> str:
    return sha256_file(path)


def repos_equal(left: Mapping[str, Any], right: Mapping[str, Any]) -> bool:
    return list(left.get("repos") or []) == list(right.get("repos") or [])


def mutate_owned_file(fixture: Path, relative_repo: str, relative_file: str, suffix: bytes) -> dict[str, str]:
    root = fixture.resolve()
    path = (fixture / relative_repo / relative_file)
    if path.is_symlink():
        raise DriverError(f"refusing to mutate symlink {path}")
    real = path.resolve()
    if root != real and root not in real.parents:
        raise DriverError(f"refusing to mutate outside owned fixture: {real}")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags)
    try:
        before = os.read(fd, 1024 * 1024)
    finally:
        os.close(fd)
    after = before + suffix
    fd = os.open(path, os.O_WRONLY | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0))
    try:
        os.write(fd, after)
    finally:
        os.close(fd)
    return {"beforeSha256": sha256_bytes(before), "afterSha256": sha256_bytes(after), "path": str(path)}


def parent_display() -> str | None:
    return os.environ.get("DISPLAY")


def assert_parent_display_unchanged(before: str | None, after: str | None) -> None:
    if before != after:
        raise DriverError(f"parent DISPLAY changed from {before!r} to {after!r}")


def probe_lines(session: Any, start: int = 0) -> list[str]:
    return [line for line in session.texts(start) if "[APP:" in line]


def interesting(lines: Sequence[str]) -> list[str]:
    keys = (
        "READY_REPOS:",
        "E2E_REPO:",
        "REPO_SELECTING:",
        "REPO_LOADED:",
        "GRAPH_LOADED:",
        "E2E_LOG:",
        "E2E_PREVIEW:",
        "PREVIEW_LOADED:",
        "BASKET:",
        "COPY_",
        "EXPORT_PLAN_READY:",
        "PASTE_",
        "COMMIT_SELECTED:",
        "RANGE:",
        "REF_FILTER:",
        "REV_TREE:",
        "TREE_",
        "FILE_TOGGLED:",
        "APPLY_IGNORED:",
    )
    return [line for line in lines if any(key in line for key in keys)]


def wait_substr(session: Any, needle: str, start: int, timeout: float) -> str:
    try:
        _index, _stamp, line = session.wait_line(lambda item: needle in item, start=start, timeout=timeout)
    except Exception as exc:
        text = str(exc)
        if "timed out" in text:
            raise OperationTimeout(text) from exc
        raise DriverError(text) from exc
    return line


def wait_any(session: Any, needles: Sequence[str], start: int, timeout: float) -> str:
    try:
        _index, _stamp, line = session.wait_line(
            lambda item: any(needle in item for needle in needles),
            start=start,
            timeout=timeout,
        )
    except Exception as exc:
        text = str(exc)
        recent = [line for line in session.texts(start) if any(token in line for token in ("REFUSED", "_ERR:", "PASTE_"))]
        if recent and "timed out" in text:
            raise UiRefusal(recent[-1]) from exc
        if "timed out" in text:
            raise OperationTimeout(text) from exc
        raise DriverError(text) from exc
    if any(token in line for token in ("REFUSED", "_ERR:")) and not any(
        ok in line for ok in ("COPY_DONE", "COPY_COMMITS_DONE", "PASTE_DONE", "PASTE_PREVIEW", "PASTE_CANCELLED", "PASTE_STALE")
    ):
        raise UiRefusal(line)
    return line


_ENV_LOCK = threading.Lock()


def open_session(
    native: Any,
    binary: Path,
    workspace: Path,
    run_dir: Path,
    timeout: float,
    extra_env: Mapping[str, str] | None = None,
) -> Any:
    run_dir.mkdir(parents=True, exist_ok=True)
    with _ENV_LOCK:
        saved = {key: os.environ.get(key) for key in extra_env or {}}
        try:
            os.environ.update(extra_env or {})
            session = native.NativeSession(
                str(binary), str(workspace), "normal", str(run_dir), True
            )
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

    def release(_app: dict[str, Any]) -> None:
        Path(session.sampler_ready_file).write_text("functional-only\n", encoding="utf-8")

    try:
        ident = session.wait_app(release, timeout=timeout)
        deadline = time.monotonic() + timeout
        expected = os.path.realpath(binary)
        while time.monotonic() < deadline:
            try:
                exe = os.path.realpath(os.readlink(f"/proc/{ident['pid']}/exe"))
            except OSError:
                exe = ""
            if exe == expected:
                break
            time.sleep(0.05)
        else:
            raise OperationTimeout("sampler gate released but the process never became the native binary")
        wait_substr(session, "[APP:WINDOW_READY]", 0, timeout)
        ready_identity = native.identity(ident["pid"])
        if not valid_app_identity(ready_identity) or any(
            ready_identity[key] != ident.get(key) for key in ("pid", "starttime")
        ):
            raise DriverError("app identity changed between sampler release and window readiness")
        if os.path.realpath(str(ready_identity.get("exe") or "")) != expected:
            raise DriverError("ready app executable does not match the frozen binary")
        session.app = {**ident, **ready_identity}
        return session
    except Exception:
        errors = session.stop()
        if errors:
            raise DriverError(f"startup failed and stop() returned cleanup errors: {errors}") from None
        raise


def discover(session: Any, expected: Sequence[str], timeout: float) -> list[str]:
    """15 E2E_REPO rows. Duplicate basenames are expected; a set compare would drop them."""
    want = Counter(expected)
    deadline = time.monotonic() + timeout
    ready_count = None
    names: list[str] = []
    while time.monotonic() < deadline:
        lines = session.texts()
        for line in lines:
            if "READY_REPOS:" in line:
                ready_count = int(line.split("READY_REPOS:", 1)[1].strip().split()[0].rstrip("]"))
        found = []
        for line in lines:
            marker = "E2E_REPO: name="
            if marker not in line or " ok=true" not in line:
                continue
            found.append(line.split(marker, 1)[1].split(" ok=", 1)[0])
        names = found
        if ready_count == len(expected) and Counter(names) == want:
            return names
        if session.proc.poll() is not None:
            raise DriverError(f"app exited during discovery with {session.proc.returncode}")
        time.sleep(0.05)
    raise OperationTimeout(
        f"discovery saw ready={ready_count} count={len(names)} unique={sorted(set(names))}"
    )


def scroll_in_view(native: Any, session: Any, win: dict[str, Any], control: str, viewport_id: str, timeout: float) -> tuple[int, int, int, int]:
    """Bounds of a control once it has stopped moving inside a scroll viewport."""
    try:
        wait_control(native, session, control, min(2, timeout))
    except Exception:
        pass  # not painted yet or scrolled out of view; the wheel loop below looks for it
    deadline = time.monotonic() + timeout
    steps = 0
    last: tuple[int, int, int, int] | None = None
    stable_at = 0.0
    while time.monotonic() < deadline and steps <= 40:
        before = len(session.lines)
        bounds = native.parse_bounds(session.texts())
        viewport = bounds.get(viewport_id)
        box = bounds.get(control)
        if viewport and box and native.visible_in(box, viewport) == 0:
            # A reflow can still move the row; only a box unchanged for a while is safe to click.
            if box != last:
                last, stable_at = box, time.monotonic()
            elif time.monotonic() - stable_at >= 0.2:
                native.assert_on_window(box, win, control)
                return box
            time.sleep(0.04)
            continue
        last = None
        if viewport is None:
            try:
                wait_control(native, session, viewport_id, min(2, max(0.0, deadline - time.monotonic())))
            except Exception as exc:
                raise MissingControl(viewport_id, "viewport has no bounds") from exc
            continue
        direction = native.visible_in(box, viewport) if box is not None else 1
        if direction == 0:
            direction = 1
        session.focus(win["wid"])
        cx = win["x"] + viewport[0] + max(1, viewport[2] // 2)
        cy = win["y"] + viewport[1] + max(1, viewport[3] // 2)
        session.x("xdotool", "mousemove", str(cx), str(cy), "click", "5" if direction > 0 else "4")
        try:
            session.wait_line(
                lambda item: f"id={control} " in item or f"id={viewport_id} " in item,
                start=before,
                timeout=min(2, max(0.0, deadline - time.monotonic())),
            )
        except Exception as exc:
            if "timed out" not in str(exc):
                raise DriverError(str(exc)) from exc
        steps += 1
    raise MissingControl(control, f"not inside {viewport_id} after {steps} wheel steps")


def wait_control(native: Any, session: Any, control: str, timeout: float) -> tuple[int, int, int, int]:
    """Bounds of a control once the app reports it; it can lag the event that enables it by a frame."""
    deadline = time.monotonic() + timeout
    while True:
        lines = session.texts()
        remaining = deadline - time.monotonic()
        if control in native.parse_bounds(lines) or remaining <= 0:
            return native.require_control(lines, control)
        time.sleep(min(0.05, remaining))


def click_control(native: Any, session: Any, win: dict[str, Any], control: str, timeout: float, viewport: str | None = None, modifier: str | None = None) -> None:
    """Click `control`; `modifier` (e.g. "ctrl") is held for the click, as the Project tree's multi-select toggle needs."""
    left_row = control.startswith(("change-", "tree-", "left-"))
    if viewport == "left-list" or (viewport is None and left_row):
        try:
            box = native.scroll_into_view(session, win, control)
        except Exception as exc:
            text = str(exc)
            if "CTRL_BOUNDS" in text or "not settled" in text or "no [APP" in text:
                raise MissingControl(control, text) from exc
            raise DriverError(text) from exc
    elif viewport:
        box = scroll_in_view(native, session, win, control, viewport, timeout)
    else:
        try:
            box = wait_control(native, session, control, timeout)
        except Exception as exc:
            raise MissingControl(control, str(exc)) from exc
    native.assert_on_window(box, win, control)
    if modifier is None:
        session.click(win, box)
        return
    session.x("xdotool", "keydown", modifier)
    try:
        session.click(win, box)
    finally:
        session.x("xdotool", "keyup", modifier)


def _current_root(session: Any) -> str | None:
    lines = [line for line in session.texts() if "REPO_SELECTING:" in line and "root=" in line]
    if not lines:
        return None
    _name, root = selecting_fields(lines[-1])
    return os.path.realpath(root)


def _settle_filter(session: Any, start: int, timeout: float) -> str:
    line = wait_substr(session, "[APP:SELECTOR_FILTER:", start, timeout)
    absorb = time.monotonic() + 0.7
    while time.monotonic() < absorb:
        time.sleep(0.05)
        fresh = [item for item in session.texts(start) if "SELECTOR_FILTER:" in item]
        if fresh:
            line = fresh[-1]
    # Bounds are logged before the popup pixels. A non-black frame can still be the previous one.
    time.sleep(1.2)
    return line


def _type_filter(session: Any, win: dict[str, Any], text: str) -> None:
    session.focus(win["wid"])
    time.sleep(0.05)
    session.x("xdotool", "type", "--delay", "30", text)


def narrow_log(native: Any, session: Any, win: dict[str, Any], name: str, timeout: float, trace: list[dict[str, Any]]) -> None:
    """The Log shows every repository of the workspace, not the toolbar's.

    Its Repository chip narrows it to the repository a step reads: the
    single-repository log whose plain commit-row ids and range counts the
    steps rely on. A row click in that menu keeps only its repository.
    """
    if "log-filter-repo" not in native.parse_bounds(session.texts()):
        trace.append({"action": "narrow-log", "skipped": "single repository"})
        return
    before = len(session.lines)
    click_control(native, session, win, "log-filter-repo", timeout)
    wait_substr(session, "[APP:LOG_MENU: Some(Repo)]", before, timeout)
    click_control(native, session, win, f"log-repo:{name}", timeout)
    try:
        index, _stamp, _line = session.wait_line(lambda item: "[APP:LOG_REPOS: n=1]" in item, start=before, timeout=timeout)
    except Exception as exc:
        raise DriverError(f"Repository chip did not narrow the log to {name}: {exc}") from exc
    line = wait_substr(session, "[APP:GRAPH_LOADED:", index, timeout)
    trace.append({"action": "narrow-log", "repo": name, "line": line})


def select_repo(native: Any, session: Any, win: dict[str, Any], basename: str, root: Path, timeout: float, trace: list[dict[str, Any]]) -> str:
    """Header selector. Project-list rows share a basename and cannot pick the root.

    Indices in pick-repo:N:name come from the live control log. A candidate is
    accepted only when a fresh REPO_SELECTING root matches the canonical path.
    """
    wanted = os.path.realpath(root)
    if _current_root(session) == wanted:
        trace.append({"action": "select-repo", "control": "already-selected", "root": wanted, "basename": basename})
        name, _root = selecting_fields([line for line in session.texts() if "REPO_SELECTING:" in line][-1])
        narrow_log(native, session, win, name, timeout, trace)
        return "already-selected"
    tried: list[str] = []
    for _attempt in range(6):
        before = len(session.lines)
        click_control(native, session, win, "btn-repo-selector", timeout)
        _settle_filter(session, before, timeout)
        click_control(native, session, win, "selector-input", timeout)
        typed = len(session.lines)
        _type_filter(session, win, basename)
        filter_line = _settle_filter(session, typed, timeout)
        bounds = native.parse_bounds(session.texts())
        candidates = [
            control
            for control in bounds
            if control.startswith("pick-repo:") and Path(control_basename(control)).name == Path(basename).name and control not in tried
        ]
        if not candidates:
            raise MissingControl(
                f"pick-repo:{basename}",
                f"no live candidate after SELECTOR_FILTER. filter={filter_line} tried={tried}",
            )
        if len(candidates) == 1:
            control = candidates[0]
        else:
            control = sorted(candidates)[0]
        tried.append(control)
        click_control(native, session, win, control, timeout)
        line = wait_substr(session, "[APP:REPO_SELECTING:", before, timeout)
        got_name, got_root = selecting_fields(line)
        if Path(got_name).name != Path(basename).name:
            raise DriverError(f"{control} selected basename {got_name}")
        if os.path.realpath(got_root) == wanted:
            wait_substr(session, f"[APP:REPO_LOADED: {got_name} ", before, timeout)
            wait_substr(session, "[APP:GRAPH_LOADED:", before, timeout)
            trace.append({"action": "select-repo", "control": control, "root": wanted, "line": line, "tried": tried})
            narrow_log(native, session, win, got_name, timeout, trace)
            return line
        trace.append({"action": "select-repo-reject", "control": control, "root": os.path.realpath(got_root), "wanted": wanted})
    raise DriverError(f"no selector candidate resolved {basename} to {wanted}; tried {tried}")


def capture_checked(native: Any, session: Any, win: dict[str, Any], name: str, timeout: float) -> dict[str, Any]:
    # A non-black crop can still be the frame from before the latest event.
    time.sleep(1.0)
    shot = session.capture(win, name, timeout=timeout)
    pid_text = session.x("xdotool", "getwindowpid", win["wid"]).strip()
    if int(pid_text) != int(session.app["pid"]):
        raise DriverError(f"screenshot window pid {pid_text} != app pid {session.app['pid']}")
    if shot["rootCropStats"]["stddev"] < 0.02:
        raise DriverError(f"screenshot {name} is a black frame")
    return {
        "png": shot["rootCrop"],
        "sha256": sha256_file(Path(shot["rootCrop"])),
        "stddev": shot["rootCropStats"]["stddev"],
        "windowPid": int(pid_text),
        "wid": win["wid"],
        "appPid": session.app["pid"],
        "appStarttime": session.app.get("starttime"),
    }


def expand_open_repo_changes(native: Any, session: Any, win: dict[str, Any], row_id: str, timeout: float, trace: list[dict[str, Any]]) -> None:
    """Changes repo rows (one per group) start collapsed; opening a repo from the
    selector expands it in every group, but the repo the app opened at startup stays
    collapsed until clicked. `row_id` is `change-row:<source>:<path>`; the row to
    expand is that repo's row under the source's group (untracked files list under
    Unstaged). Then the directories above the file, which also start collapsed.

    Bounds carry no expansion state, so the repo row is clicked and its
    REPO_CHANGES_COLLAPSED line read: `collapsed=true` means it was open with the file
    out of view (or inside a closed folder), so it is clicked once more. With many
    repos the row may be out of view; the list is swept for it."""
    lines = session.texts()
    selecting = [line for line in lines if "REPO_SELECTING:" in line]
    loaded = [line for line in lines if "[APP:REPO_LOADED: " in line]
    if selecting:
        name, _root = selecting_fields(selecting[-1])
    elif loaded:
        name = loaded[-1].split("[APP:REPO_LOADED: ", 1)[1].split(" files=", 1)[0]
    else:
        return
    group = native.change_row_group(row_id)
    node = f"change-repo:{group}:{name}"

    def find_node() -> list[str] | None:
        bounds = native.parse_bounds(session.texts())
        if row_id in bounds:
            return None
        return [node] if node in bounds else []

    deadline = time.monotonic() + min(2.0, timeout)
    while (found := find_node()) == [] and time.monotonic() < deadline:
        time.sleep(0.1)
    # Only a multi-repo workspace has repo rows.
    if found == [] and any("id=change-repo:" in line for line in session.texts()):
        found = native.scroll_to_change_dirs(session, win, find_node)
    if found is None:
        return
    if found:
        needle = f"[APP:REPO_CHANGES_COLLAPSED: {group} {name} collapsed="
        for _click in range(2):
            before = len(session.lines)
            click_control(native, session, win, node, timeout)
            line = wait_substr(session, needle, before, timeout)
            if "collapsed=false" in line:
                break
        trace.append({"action": "expand-repo-changes", "control": node})
    opened = native.expand_change_dirs(session, win, row_id, name, timeout)
    if opened:
        trace.append({"action": "expand-change-dirs", "dirs": opened})


def preview_change(native: Any, session: Any, win: dict[str, Any], kind: str, path: str, timeout: float, trace: list[dict[str, Any]], check: bool) -> None:
    row_id, chk_id = change_control(kind, path)
    native.show_changes(session, win)
    expand_open_repo_changes(native, session, win, row_id, timeout, trace)
    before = len(session.lines)
    try:
        click_control(native, session, win, row_id, timeout)
    except MissingControl:
        if kind not in ("working", "unstaged"):
            raise
        raise MissingControl(row_id, "working/index/delete row was not rendered")
    source_key = {"working": "unstaged", "index": "staged", "unstaged": "unstaged", "staged": "staged"}[kind]
    preview_kind = native.PREVIEW_KIND[source_key]
    wait_substr(session, f"[APP:PREVIEW_LOADED: {path}]", before, timeout)
    wait_substr(session, f"[APP:E2E_PREVIEW: source={preview_kind} ", before, timeout)
    trace.append({"action": "preview", "control": row_id, "kind": kind, "path": path})
    if not check:
        return
    before = len(session.lines)
    click_control(native, session, win, chk_id, timeout)
    wait_substr(session, "[APP:BASKET: n=", before, timeout)
    trace.append({"action": "check", "control": chk_id, "path": path})


def click_until_logged(session: Any, win: dict[str, Any], box: tuple[int, int, int, int], needles: Sequence[str], timeout: float, tries: int = 4) -> int:
    """Click until the app logs one of `needles`; a button that is not enabled yet drops the click.

    Every wait starts at the first click, so an accepted click is never sent again.
    Returns that start index; after the last try the caller's own wait reports the failure.
    """
    before = len(session.lines)
    for _ in range(tries):
        session.click(win, box)
        try:
            session.wait_line(lambda item: any(needle in item for needle in needles), start=before, timeout=min(1.5, timeout))
            break
        except Exception as exc:
            if "timed out" not in str(exc):
                raise DriverError(str(exc)) from exc
    return before


def copy_from_button(native: Any, session: Any, win: dict[str, Any], timeout: float, trace: list[dict[str, Any]]) -> bytes:
    sentinel = f"SNIP-COLLAB-SENTINEL-{uuid.uuid4().hex}\n".encode()
    session.set_clipboard(sentinel)
    if session.read_clipboard() != sentinel:
        raise ClipboardMismatch("sentinel did not stick on the source clipboard")
    box = wait_control(native, session, "btn-copy", timeout)
    native.assert_on_window(box, win, "btn-copy")
    before = click_until_logged(session, win, box, ("[APP:COPY_PREP:", "[APP:COPY_BUSY]", "[APP:COPY_DONE:", "[APP:COPY_REFUSED:"), timeout)
    line = wait_any(session, ("[APP:COPY_DONE:", "[APP:COPY_REFUSED:"), before, timeout)
    if "COPY_REFUSED" in line:
        raise UiRefusal(line)
    trace.append({"action": "copy", "control": "btn-copy", "line": line})
    deadline = time.monotonic() + timeout
    payload = b""
    while time.monotonic() < deadline:
        payload = session.read_clipboard()
        if payload and sentinel not in payload:
            break
        time.sleep(0.05)
    else:
        raise OperationTimeout("source clipboard still held the sentinel after COPY_DONE")
    return payload


def paste_preview(native: Any, session: Any, win: dict[str, Any], timeout: float, trace: list[dict[str, Any]]) -> str:
    box = wait_control(native, session, "btn-paste", timeout)
    native.assert_on_window(box, win, "btn-paste")
    before = len(session.lines)
    session.click(win, box)
    line = wait_any(session, ("[APP:PASTE_PREVIEW:", "[APP:PASTE_ERR:"), before, timeout)
    if "PASTE_ERR" in line:
        raise UiRefusal(line)
    trace.append({"action": "paste-preview", "control": "btn-paste", "line": line})
    return line


def click_overwrites(native: Any, session: Any, win: dict[str, Any], paths: Sequence[str] | None, timeout: float, trace: list[dict[str, Any]]) -> list[str]:
    clicked: list[str] = []
    # PASTE_PREVIEW is logged before the frame that paints the panel.
    wait_control(native, session, "paste-items", timeout)
    idle = 0
    while idle < 4:
        bounds = native.parse_bounds(session.texts())
        ids = [control for control in bounds if control.startswith("paste-overwrite:")]
        if paths is not None:
            ids = [control for control in ids if control.split(":", 1)[1] in paths]
        pending = [control for control in ids if control not in clicked]
        if not pending:
            viewport = bounds.get("paste-items") or bounds.get("paste-row:" + (paths[0] if paths else ""))
            if "paste-items" not in bounds and not ids:
                break
            host = bounds.get("paste-items")
            if host is None and ids:
                host = bounds[ids[0]]
            if host is None:
                break
            session.focus(win["wid"])
            session.x(
                "xdotool",
                "mousemove",
                str(win["x"] + host[0] + host[2] // 2),
                str(win["y"] + host[1] + min(host[3] // 2, 20)),
                "click",
                "5",
            )
            time.sleep(0.1)
            idle += 1
            continue
        idle = 0
        control = pending[0]
        box = bounds[control]
        try:
            native.assert_on_window(box, win, control)
        except Exception:
            idle += 1
            continue
        before = len(session.lines)
        session.click(win, box)
        line = wait_substr(session, "[APP:PASTE_TOGGLED:", before, timeout)
        if "state=true" not in line:
            raise DriverError(f"overwrite toggle did not turn on: {line}")
        clicked.append(control)
        trace.append({"action": "overwrite-on", "control": control, "line": line})
    if paths is not None:
        missing = [path for path in paths if f"paste-overwrite:{path}" not in clicked]
        if missing and "paste-items" in native.parse_bounds(session.texts()):
            for path in list(missing):
                ctrl = f"paste-overwrite:{path}"
                try:
                    box = scroll_in_view(native, session, win, ctrl, "paste-items", timeout)
                    before = len(session.lines)
                    session.click(win, box)
                    line = wait_substr(session, "[APP:PASTE_TOGGLED:", before, timeout)
                    if "state=true" in line:
                        clicked.append(ctrl)
                        trace.append({"action": "overwrite-on", "control": ctrl, "line": line})
                except Exception:
                    pass
        missing = [path for path in paths if f"paste-overwrite:{path}" not in clicked]
        if missing:
            raise MissingControl("paste-overwrite:" + ",".join(missing), "authorized overwrite control was not clicked")
    return clicked


def parse_paste_map_candidates(lines: Sequence[str]) -> dict[str, list[tuple[int, str]]]:
    """Extract candidate paths from [APP:PASTE_MAP_CANDIDATE: prefix=... idx=... path=...]."""
    prefix_candidates: dict[str, list[tuple[int, str]]] = {}
    for line in lines:
        m = PASTE_MAP_CANDIDATE_RE.search(line)
        if not m:
            continue
        prefix = m.group("prefix")
        idx = int(m.group("idx"))
        path = m.group("path")
        prefix_candidates.setdefault(prefix, []).append((idx, path))
    return prefix_candidates


def resolve_paste_mappings(
    native: Any,
    session: Any,
    win: dict[str, Any],
    manifest: Mapping[str, Any],
    dest_repo_id: str,
    fixture: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    start_line: int = 0,
) -> None:
    """Resolve destination prefix mappings using manifest repo ID and canonical root.

    Scopes candidate logs to current generation using the cursor saved before Paste.
    Disambiguates duplicate basenames by strictly matching canonical root paths.
    No lone-button fallback: must match canonical root.
    Waits for async mapping replan and validates that PASTE_MAPPED prefix and destination match.
    """
    gen_start = start_line
    current_lines = session.texts(gen_start)
    bounds = native.parse_bounds(session.texts())
    candidate_lines = [line for line in current_lines if "PASTE_MAP_CANDIDATE:" in line]
    pick_controls = [c for c in bounds if c.startswith("paste-map-pick:")]

    if "paste-mappings" not in bounds and not candidate_lines and not pick_controls:
        return

    dest_repo = repo_by_id(manifest, dest_repo_id)
    canonical_dest_root = str(os.path.realpath(fixture / dest_repo["relativePath"]))

    prefix_candidates = parse_paste_map_candidates(current_lines)

    # Detect all active prefixes needing assignment from current generation candidates and pick controls
    prefixes = set(prefix_candidates.keys())
    for control in bounds:
        if control.startswith("paste-map-pick:"):
            parts = control.split(":")
            if len(parts) >= 3 and parts[1] in prefix_candidates:
                prefixes.add(parts[1])

    for prefix in sorted(prefixes):
        target_idx: int | None = None
        candidates = prefix_candidates.get(prefix, [])
        for idx, path in candidates:
            if str(os.path.realpath(path)) == canonical_dest_root:
                target_idx = idx
                break

        if target_idx is None:
            raise MissingControl(
                f"paste-map:{prefix}",
                f"cannot resolve destination root {canonical_dest_root} among current generation candidates {candidates}",
            )

        control = f"paste-map-pick:{prefix}:{target_idx}"
        viewport = "paste-items" if "paste-items" in bounds else None
        before = len(session.lines)
        click_control(native, session, win, control, timeout, viewport=viewport)
        line = wait_any(session, ("[APP:PASTE_MAPPED:", "[APP:PASTE_ERR:"), before, timeout)
        if "PASTE_ERR" in line:
            raise UiRefusal(line)

        # Validate that emitted PASTE_MAPPED prefix and dest strictly match (spaces and Unicode preserved)
        m = PASTE_MAPPED_RE.search(line)
        if not m:
            raise DriverError(f"PASTE_MAPPED missing mandatory fields: {line}")
        mapped_prefix = m.group("prefix")
        mapped_dest = m.group("dest")
        if mapped_prefix != prefix:
            raise DriverError(f"PASTE_MAPPED prefix mismatch: expected {prefix!r}, got {mapped_prefix!r} in {line}")
        if str(os.path.realpath(mapped_dest)) != canonical_dest_root:
            raise DriverError(f"PASTE_MAPPED dest mismatch: expected {canonical_dest_root!r}, got {mapped_dest!r} in {line}")

        trace.append({"action": "paste-map", "control": control, "line": line, "dest": canonical_dest_root})


def apply_or_cancel(native: Any, session: Any, win: dict[str, Any], control: str, timeout: float, trace: list[dict[str, Any]]) -> str:
    box = wait_control(native, session, control, timeout)
    native.assert_on_window(box, win, control)
    if control == "btn-cancel":
        accepted = ("[APP:PASTE_CANCELLED]", "[APP:PASTE_BUSY:")
    else:
        accepted = ("[APP:PASTE_APPLYING]", "[APP:PASTE_DONE:", "[APP:PASTE_STALE_DETECTED:", "[APP:PASTE_ERR:", "[APP:APPLY_IGNORED:", "[APP:PASTE_BUSY:")
    before = click_until_logged(session, win, box, accepted, timeout)
    if control == "btn-cancel":
        line = wait_any(session, ("[APP:PASTE_CANCELLED]", "[APP:PASTE_BUSY:"), before, timeout)
    else:
        line = wait_any(
            session,
            ("[APP:PASTE_DONE:", "[APP:PASTE_STALE_DETECTED:", "[APP:PASTE_ERR:", "[APP:APPLY_IGNORED:"),
            before,
            timeout,
        )
    trace.append({"action": control, "line": line})
    return line


def save_payload(path: Path, payload: bytes) -> dict[str, Any]:
    path.write_bytes(payload)
    return {"path": str(path), "sha256": sha256_bytes(payload), "bytes": len(payload)}


def select_fixed_oid(
    native: Any,
    session: Any,
    win: dict[str, Any],
    repo: Path,
    rev: str,
    path: str,
    oid: str,
    timeout: float,
    trace: list[dict[str, Any]],
) -> None:
    """Select a pinned blob via historical tree navigation and rev-chk:<commit>:<path>.

    Per 4c38d79 smoke.rs:
    1. Select commit row by full/short OID.
    2. Assert active root matches expected canonical repo root.
    3. Assert Git blob OID from manifest matches Git oracle.
    4. Click 'btn-browse-tree:<full SHA>' after its completed-layout probe -> wait REV_TREE matching selected commit and E2E_TREE root.
    5. Expand parent directories via rev-row:<parent> (do not suppress MissingControl).
    6. Navigate to file row: rev-row:<path> -> wait PREVIEW_LOADED and E2E_PREVIEW.
    7. Click historical checkbox: rev-chk:<commit>:<path> -> wait BASKET.
    """
    commit = git_read(repo, ["rev-parse", "--verify", f"{rev}^{{commit}}"], timeout).strip()
    selection_start = len(session.lines)
    click_commit_row(native, session, win, repo, commit, timeout, trace)
    active_root = _current_root(session)
    expected_root = str(os.path.realpath(repo))
    if not active_root:
        raise DriverError(f"active root missing after selecting commit {commit}")
    if active_root != expected_root:
        raise DriverError(f"selected commit {commit} on wrong root {active_root} != {expected_root}")

    # Assert Git blob OID from manifest against actual Git repository blob
    oracle_blob_oid = git_read(repo, ["rev-parse", "--verify", f"{commit}:{path}"], timeout).strip()
    if oracle_blob_oid != oid:
        raise DriverError(f"manifest blob OID {oid} does not match git oracle {oracle_blob_oid} for {commit}:{path}")

    # The revision-qualified probe appears only after the commit header finishes loading.
    wait_substr(session, f"[APP:E2E_PREVIEW: source=commit_diff rev={commit} ", selection_start, timeout)
    short_sha = commit[:7]
    before = len(session.lines)
    click_control(native, session, win, f"btn-browse-tree:{commit}", timeout)
    wait_substr(session, f"[APP:REV_TREE: {short_sha}]", before, timeout)
    wait_substr(session, f"[APP:E2E_TREE: rev={short_sha} dir=/", before, timeout)
    trace.append({"action": "browse-tree", "commit": commit, "short": short_sha})

    # Expand any directory components in historical tree (do NOT suppress MissingControl)
    parts = path.split("/")
    for i in range(1, len(parts)):
        parent = "/".join(parts[:i])
        parent_before = len(session.lines)
        click_control(native, session, win, f"rev-row:{parent}", timeout, viewport="left-list")
        wait_substr(session, f"[APP:TREE_EXPANDED: {parent}]", parent_before, timeout)
        trace.append({"action": "expand-rev", "path": parent})

    # Navigate to the file row: rev-row:<path>
    nav_id, chk_id = fixed_oid_controls(commit, path)
    before = len(session.lines)
    click_control(native, session, win, nav_id, timeout, viewport="left-list")
    wait_substr(session, f"[APP:PREVIEW_LOADED: {path}]", before, timeout)
    wait_substr(session, f"[APP:E2E_PREVIEW: source=commit_file rev={commit} path={path} ", before, timeout)
    trace.append({"action": "rev-nav", "control": nav_id, "path": path, "commit": commit, "oid": oid})

    # Click historical file checkbox rev-chk:<commit>:<path>
    before = len(session.lines)
    click_control(native, session, win, chk_id, timeout, viewport="left-list")
    wait_substr(session, "[APP:BASKET: n=", before, timeout)
    trace.append({"action": "check-fixed-oid", "control": chk_id, "commit": commit, "path": path, "oid": oid})


def leave_rev_tree_if_open(
    native: Any,
    session: Any,
    win: dict[str, Any],
    timeout: float,
    trace: list[dict[str, Any]],
) -> None:
    """Exit historical tree browsing if currently open."""
    rev_state = None
    for line in reversed(session.texts()):
        if "[APP:REV_TREE: " in line:
            if "[APP:REV_TREE: off]" in line:
                rev_state = "off"
            else:
                rev_state = "on"
            break
    if rev_state == "on":
        before = len(session.lines)
        click_control(native, session, win, "btn-leave-tree", timeout)
        wait_substr(session, "[APP:REV_TREE: off]", before, timeout)
        trace.append({"action": "leave-rev-tree"})
        # browse_commit_tree selects Project without TAB_SWITCHED; leaving it
        # does not restore Changes. The shared helper's cached tab is stale.
        before = len(session.lines)
        click_control(native, session, win, "rail-changes", timeout)
        line = wait_substr(session, "[APP:TAB_SWITCHED: GitChanges visible=true ", before, timeout)
        trace.append({"action": "restore-changes", "line": line})


def demonstrate_fixed_oid(
    native: Any,
    session: Any,
    win: dict[str, Any],
    repo: Path,
    rev: str,
    path: str,
    oid: str,
    timeout: float,
    trace: list[dict[str, Any]],
) -> None:
    select_fixed_oid(native, session, win, repo, rev, path, oid, timeout, trace)


def click_ref(native: Any, session: Any, win: dict[str, Any], ref: str, timeout: float, trace: list[dict[str, Any]]) -> None:
    control = f"ref:{ref}"
    before = len(session.lines)
    click_control(native, session, win, control, timeout, viewport="log-list")
    line = wait_substr(session, f"[APP:REF_FILTER: {ref}]", before, timeout)
    wait_substr(session, "[APP:GRAPH_LOADED:", before, timeout)
    trace.append({"action": "ref-filter", "control": control, "line": line})


def unique_short(repo: Path, oid: str, timeout: float) -> str:
    short = oid[:7]
    listed = git_read(repo, ["rev-list", "--all"], timeout).split()
    if sum(item.startswith(short) for item in listed) != 1:
        raise DriverError(f"short oid {short} is not unique")
    return short


def click_commit_row(native: Any, session: Any, win: dict[str, Any], repo: Path, oid: str, timeout: float, trace: list[dict[str, Any]]) -> str:
    short = unique_short(repo, oid, timeout)
    control = f"commit-row:{short}"
    before = len(session.lines)
    click_control(native, session, win, control, timeout, viewport="log-list")
    wait_substr(session, f"[APP:COMMIT_SELECTED: {short}]", before, timeout)
    deadline = time.monotonic() + min(timeout, 8)
    while time.monotonic() < deadline:
        if any(oid in line for line in session.texts(before)):
            break
        time.sleep(0.05)
    else:
        raise DriverError(f"clicked {control} but no fresh log line carried full oid {oid}")
    trace.append({"action": "commit-click", "control": control, "oid": oid})
    return control


def shift_click_commit(native: Any, session: Any, win: dict[str, Any], repo: Path, oid: str, timeout: float, trace: list[dict[str, Any]]) -> str:
    short = unique_short(repo, oid, timeout)
    control = f"commit-row:{short}"
    box = scroll_in_view(native, session, win, control, "log-list", timeout)
    session.focus(win["wid"])
    session.x("xdotool", "keydown", "shift")
    try:
        ax = win["x"] + box[0] + box[2] // 2
        ay = win["y"] + box[1] + box[3] // 2
        session.x("xdotool", "mousemove", str(ax), str(ay), "click", "1")
    finally:
        session.x("xdotool", "keyup", "shift")
    trace.append({"action": "commit-shift-click", "control": control, "oid": oid})
    return control


def expand_rev_dir(native: Any, session: Any, win: dict[str, Any], directory: str, timeout: float, trace: list[dict[str, Any]]) -> None:
    before = len(session.lines)
    click_control(native, session, win, f"rev-row:{directory}", timeout, viewport="left-list")
    wait_substr(session, f"[APP:TREE_EXPANDED: {directory}]", before, timeout)
    trace.append({"action": "expand-rev", "path": directory})


def copy_commits(native: Any, session: Any, win: dict[str, Any], timeout: float, trace: list[dict[str, Any]], sentinel: bytes | None = None) -> tuple[bytes, str]:
    sentinel = sentinel or f"SNIP-COLLAB-SENTINEL-{uuid.uuid4().hex}\n".encode()
    session.set_clipboard(sentinel)
    if session.read_clipboard() != sentinel:
        raise ClipboardMismatch("commit sentinel did not stick on the source clipboard")
    box = wait_control(native, session, "btn-copy-commits", timeout)
    native.assert_on_window(box, win, "btn-copy-commits")
    before = click_until_logged(
        session, win, box,
        ("[APP:COPY_COMMITS_PREP:", "[APP:COPY_COMMITS_DONE:", "[APP:COPY_COMMITS_ERR:", "[APP:COPY_COMMITS_REFUSED:"),
        timeout,
    )
    line = wait_any(session, ("[APP:COPY_COMMITS_DONE:", "[APP:COPY_COMMITS_ERR:", "[APP:COPY_COMMITS_REFUSED:"), before, timeout)
    trace.append({"action": "copy-commits", "line": line})
    if "COPY_COMMITS_DONE" not in line:
        raise UiRefusal(line)
    deadline = time.monotonic() + timeout
    payload = b""
    while time.monotonic() < deadline:
        payload = session.read_clipboard()
        if payload and sentinel not in payload:
            return payload, line
        time.sleep(0.05)
    raise OperationTimeout("commit clipboard still held the sentinel")


def select_commit_range(native: Any, session: Any, win: dict[str, Any], repo: Path, selection: Mapping[str, Any], timeout: float, trace: list[dict[str, Any]]) -> None:
    """Click the oldest full OID, then shift-click the tip. Accept only a fresh RANGE of that count."""
    oids = [str(item) for item in selection["oids"]]
    if not oids:
        raise DriverError("commit selection is empty")
    click_commit_row(native, session, win, repo, oids[0], timeout, trace)
    if len(oids) == 1:
        return
    before = len(session.lines)
    shift_click_commit(native, session, win, repo, oids[-1], timeout, trace)
    line = wait_substr(session, "[APP:RANGE: commits=", before, timeout)
    wanted = f"commits={len(oids)}"
    if wanted not in line:
        raise DriverError(f"shift-click range {line.strip()} is not {wanted}; refusing to copy")
    trace.append({"action": "range", "line": line, "oids": oids})


def valid_app_identity(ident: Any) -> bool:
    return isinstance(ident, dict) and all(
        type(ident.get(key)) is int and ident[key] > 0 for key in ("pid", "starttime")
    )


def failure_diagnostics(native: Any, session: Any, name: str) -> dict[str, Any]:
    """Capture observation before teardown; diagnostic failure never hides the cause."""
    result: dict[str, Any] = {"errors": []}
    try:
        result["logTail"] = session.texts()[-30:]
    except Exception as exc:
        result["errors"].append(f"log: {exc}")
    try:
        app = session.app
        result["app"] = native.identity(app["pid"])
        if not valid_app_identity(result["app"]) or any(result["app"][key] != app[key] for key in ("pid", "starttime")):
            result["errors"].append("saved app identity is no longer alive; no window capture attempted")
            return result
        proc = Path(f"/proc/{app['pid']}")
        result["wchan"] = (proc / "wchan").read_text().strip()
        result["processState"] = [
            line for line in (proc / "status").read_text().splitlines()
            if line.startswith(("State:", "Threads:"))
        ]
    except Exception as exc:
        result["errors"].append(f"process: {exc}")
    try:
        result["focusedWindowBeforeCapture"] = session.x("xdotool", "getwindowfocus", timeout=3).strip()
        win = session.window(timeout=3)
        result["window"] = win
        result["captureSetsFocus"] = True
        result["screenshot"] = capture_checked(native, session, win, name, 3)
    except Exception as exc:
        result["errors"].append(f"window: {exc}")
    return result


def stop_one(native: Any, session: Any) -> dict[str, Any]:
    """Gracefully terminate the application via Ctrl+Q, verify clean exit, and reap wrappers."""
    owned: list[dict[str, Any]] = []
    app = None
    app_descendants: list[dict[str, Any]] = []
    cleanup_errors: list[str] = []
    exit_code: int | None = None
    forced = False
    drained = False
    quit_timeout_evidence = None

    try:
        session.remember_owned()
        owned = list(session.owned)
        app = getattr(session, "app", None)
        if not valid_app_identity(app):
            cleanup_errors.append("session has no valid app identity")
        else:
            app_descendants.append(app)
            app_pids = session.app_tree_pids()
            for p in app_pids:
                if p == app["pid"]:
                    continue
                ident = native.identity(p)
                if valid_app_identity(ident):
                    app_descendants.append(ident)
                else:
                    cleanup_errors.append(f"no valid identity for app descendant {p}")
    except Exception as exc:
        cleanup_errors.append(f"remember owned error: {exc}")

    proc = getattr(session, "proc", None)
    if proc is not None and proc.poll() is not None:
        exit_code = proc.poll()
        cleanup_errors.append(f"app process had prematurely exited with code {exit_code} before quit requested")
    elif proc is not None:
        quit_start = len(session.lines)
        # Request Ctrl+Q on the app window
        try:
            win = session.window(timeout=3.0)
            session.key(win["wid"], "ctrl+q")
        except Exception as exc:
            cleanup_errors.append(f"failed sending ctrl+q: {exc}")

        # Require normal app exit 0
        try:
            exit_code = proc.wait(timeout=10.0)
            if exit_code != 0:
                cleanup_errors.append(f"app process exited with non-zero code {exit_code} on Ctrl+Q")
        except subprocess.TimeoutExpired:
            forced = True
            cleanup_errors.append("app did not exit within 10s after Ctrl+Q (forced quit required)")
            quit_timeout_evidence = failure_diagnostics(native, session, "quit-timeout")

        # Verify fresh drained zero-job evidence
        texts = session.texts(quit_start)
        drained = any(
            ("phase=drained intent=quit" in line or "LIFE: drained intent=quit" in line)
            and "jobs=0" in line
            for line in texts
        )
        if not drained:
            cleanup_errors.append("missing fresh lifecycle drained zero-job evidence (phase=drained intent=quit jobs=0)")

    # Observe no surviving app descendants BEFORE stop teardown
    survivors = []
    for ident in app_descendants:
        if not isinstance(ident, dict) or "pid" not in ident:
            continue
        pid = ident["pid"]
        starttime = ident.get("starttime")
        alive = native.read_proc_starttime(pid) == starttime
        if alive:
            survivors.append({"pid": pid, "starttime": starttime})
            cleanup_errors.append(f"surviving app descendant pid {pid} starttime {starttime} before stop teardown")

    # Now clean up harness-owned wrappers (Xvfb, xclip, pipes, temp directories)
    try:
        stop_problems = session.stop()
        if stop_problems:
            cleanup_errors.extend(str(p) for p in stop_problems)
    except Exception as exc:
        cleanup_errors.append(f"session.stop error: {exc}")

    return {
        "app": app,
        "appDescendants": app_descendants,
        "owned": owned,
        "errors": cleanup_errors,
        "survivors": survivors,
        "appExit": exit_code,
        "forced": forced,
        "drained": drained,
        "quitTimeoutEvidence": quit_timeout_evidence,
    }


def relative_shot(output: Path, shot: Mapping[str, Any]) -> dict[str, Any]:
    copied = dict(shot)
    png = shot.get("png")
    if png:
        copied["png"] = os.path.relpath(png, output)
    return copied


def new_commit_metadata(before: Mapping[str, Any], after: Mapping[str, Any], repo_id: str) -> list[dict[str, Any]]:
    old = {item["oid"] for item in next(repo["commits"] for repo in before["repos"] if repo["repoId"] == repo_id)}
    found = []
    for commit in next(repo["commits"] for repo in after["repos"] if repo["repoId"] == repo_id):
        if commit["oid"] in old:
            continue
        found.append(
            {
                "oid": commit["oid"],
                "tree": commit["tree"],
                "parents": commit["parents"],
                "authorName": commit["authorName"],
                "authorEmail": commit["authorEmail"],
                "authorTime": commit["authorTime"],
                "message": commit["message"],
            }
        )
    return found


def run_steps(config: Mapping[str, Any]) -> dict[str, Any]:
    native = config["native"]
    manifest = config["manifest"]
    output = Path(config["output"])
    timeout = float(config["timeout"])
    names = {
        "a": ui_repo_names([repo for repo in manifest["repos"] if machine_of(repo) == "a"]),
        "b": ui_repo_names([repo for repo in manifest["repos"] if machine_of(repo) == "b"]),
    }
    report: dict[str, Any] = {
        "acceptanceComplete": False,
        "historicalBinary": True,
        "releaseClaimed": False,
        "ciGreen": False,
        "merged": False,
        "memoryAbsenceClaimed": False,
        "synthetic": False,
        "syntheticCompleteness": False,
        "scope": "linux-functional" if config.get("phase") == "all" and not config.get("steps") else config.get("phase", "pilot"),
        "incomplete": True,
        "binary": config["binary"],
        "binarySha256": config["binary"]["sha256"],
        "datasetHash": config["dataset_hash"],
        "fixture": {"path": config["fixture"], "datasetHash": config["dataset_hash"], "preserved": True},
        "helpers": config["helpers"],
        "driverHead": config.get("driverHead"),
        "platform": {"executed": ["linux"], "required": ["linux", "windows", "macos"], "complete": False},
        "memoryGate": {
            "executed": False,
            "reason": "Sampler gate was released for a functional UI pilot. This run is not a memory or fd/thread gate.",
        },
        "graph": graph_from_lines([]),
        "ownedFixtures": [],
        "steps": [],
        "blockers": [],
    }
    graph_lines: list[str] = []
    stop_widening = False
    for step_id in config["step_ids"]:
        if stop_widening:
            skipped = blank_step(step_id)
            skipped["status"] = "blocked"
            skipped["failures"] = ["not started after an environment blocker"]
            report["steps"].append(skipped)
            continue
        step = next(item for item in manifest["steps"] if item["id"] == step_id)
        record, fatal = run_one(native, manifest, step, names, output, timeout, config)
        report["steps"].append(record)
        report["ownedFixtures"].extend(record.get("ownedFixtures") or [])
        graph_lines.extend(record.get("probeGenerations") or [])
        write_json(output / "report.json", report)
        print(f"step {step_id}: {record['status']}", flush=True)
        if fatal:
            stop_widening = True
            report["blockers"].append(record["failures"][-1] if record["failures"] else "environment failure")
    report["graph"] = graph_from_lines(graph_lines)
    report["graph"]["screenshots"] = [
        step["screenshots"].get("graph")
        for step in report["steps"]
        if isinstance(step.get("screenshots"), dict) and step["screenshots"].get("graph")
    ]
    report["counts"] = count_steps(report["steps"])
    func_problems = functional_problems(report)
    rel_problems = release_problems(report)
    report["functionalProblems"] = func_problems
    report["releaseRequirements"] = rel_problems
    report["acceptanceProblems"] = rel_problems
    report["acceptanceComplete"] = False
    report["exitCode"] = functional_exit_code(report)
    report["incomplete"] = bool(report["exitCode"] != 0)
    write_json(output / "report.json", report)
    (output / "report.md").write_text(render_markdown(report), encoding="utf-8")
    return report


def count_steps(steps: Sequence[Mapping[str, Any]]) -> dict[str, int]:
    counts = {"passed": 0, "failed": 0, "blocked": 0, "timeout": 0, "refused": 0, "other": 0}
    for step in steps:
        status = str(step.get("status"))
        if status in counts:
            counts[status] += 1
        else:
            counts["other"] += 1
    return counts


def run_one(
    native: Any,
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    output: Path,
    timeout: float,
    config: Mapping[str, Any],
) -> tuple[dict[str, Any], bool]:
    step_id = str(step["id"])
    record = blank_step(step_id)
    record["kind"] = step.get("kind")
    record["reason"] = step.get("reason")
    record["hashes"] = {
        "binary": config["binary"],
        "datasetHash": config["dataset_hash"],
        "helpers": config["helpers"]["files"],
    }
    step_dir = output / "steps" / step_id
    fixture = step_dir / "fixture"
    sessions: list[Any] = []
    fatal = False
    trace: list[dict[str, Any]] = []
    try:
        copy_fixture(Path(config["fixture"]), fixture)
        record["ownedFixtures"] = [str(fixture)]
        before_path = step_dir / "before.json"
        before = snapshot_file(fixture, before_path)
        record["snapshots"]["before"] = {"path": str(before_path), "sha256": snapshot_digest(before_path)}
        try:
            compare_step(str(fixture), step_id, before, "initial")
            record["compare"]["initial"] = "ok"
        except CompareError as exc:
            raise DriverError(f"owned baseline is dirty: {exc}") from exc
        workspaces = {
            "a": fixture / "machine-a",
            "b": fixture / "machine-b",
        }
        extra_envs = {}
        if step.get("id") == "neg-stale-source" or step.get("reason") == "stale-source":
            extra_envs["a"] = {"SNIP_NATIVE_E2E_EXPORT_HOLD_FILE": str(step_dir / "export_hold.signal")}

        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = {
                name: pool.submit(
                    open_session,
                    native,
                    Path(config["binary"]["path"]),
                    workspaces[name],
                    step_dir / f"session-{name}",
                    timeout,
                    extra_envs.get(name),
                )
                for name in ("a", "b")
            }
            opened = {}
            errors = []
            for name, future in futures.items():
                try:
                    opened[name] = future.result()
                except Exception as exc:
                    errors.append(f"{name}: {exc}")
            sessions.extend(opened.values())
            if errors:
                fatal = True
                raise DriverError("session startup failed: " + "; ".join(errors))
        for name, session in opened.items():
            expected = list(names[name].values())
            found = discover(session, expected, timeout)
            trace.append({"action": "discover", "machine": name, "repos": found})
            if len(found) != 15 or Counter(found) != Counter(expected):
                raise DriverError(f"machine {name} showed {found}")
        record["source"] = source_identity(step, manifest, names)
        execute_step(native, opened, manifest, step, names, fixture, step_dir, timeout, trace, record, before)
        if record["status"] == "not-run":
            record["status"] = "passed"
    except Exception as exc:
        status = classify_exception(exc)
        record["status"] = status
        record["failures"].append(str(exc))
        record["traceback"] = traceback.format_exc(limit=8)
        record["failureEvidence"] = [failure_diagnostics(native, session, "failure-before-cleanup") for session in sessions]
        if isinstance(exc, (OperationTimeout, DriverError)) and "session startup" in str(exc):
            fatal = True
    finally:
        cleanup_errors: list[str] = []
        survivors: list[dict[str, Any]] = []
        owned_notes = []
        app_records: list[dict[str, Any]] = []
        app_exits: list[int | None] = []
        forced_flags: list[bool] = []
        for session in sessions:
            try:
                stopped = stop_one(native, session)
            except Exception as exc:
                stopped = {
                    "errors": [str(exc)],
                    "survivors": [],
                    "owned": [],
                    "app": None,
                    "appExit": None,
                    "forced": True,
                    "drained": False,
                }
            cleanup_errors.extend(stopped["errors"])
            survivors.extend(stopped["survivors"])
            owned_notes.append({"app": stopped.get("app"), "owned": stopped.get("owned")})
            app_records.append(stopped)
            app_exits.append(stopped.get("appExit"))
            forced_flags.append(bool(stopped.get("forced")))
        has_null = any(e is None for e in app_exits)
        max_exit = None if has_null else (max(app_exits) if app_exits else 0)
        any_forced = any(forced_flags)
        all_drained = bool(app_records and all(r.get("drained") is True for r in app_records))
        record["cleanup"] = {
            "errors": cleanup_errors,
            "survivors": survivors,
            "processes": owned_notes,
            "apps": app_records,
            "appExit": max_exit,
            "forced": any_forced,
            "drained": all_drained,
        }
        if cleanup_errors or survivors or max_exit != 0 or any_forced or has_null or not all_drained:
            record["failures"].append("cleanup failed")
            if record["status"] == "passed":
                record["status"] = "failed"
        record["actions"] = trace
        try:
            if fixture.is_dir() and "after" not in record["snapshots"]:
                after_path = step_dir / "after.json"
                after = snapshot_file(fixture, after_path)
                record["snapshots"]["after"] = {"path": str(after_path), "sha256": snapshot_digest(after_path)}
                if record["snapshots"].get("before") and repos_equal(before, after) and not step.get("writes"):
                    record["compare"]["noWrite"] = "after-equals-before"
                elif record["status"] == "passed" and step.get("writes"):
                    record["status"] = "failed"
                    record["failures"].append("passed step has no compare result")
        except Exception as exc:
            record["failures"].append(f"final snapshot failed: {exc}")
            if record["status"] == "passed":
                record["status"] = "failed"
        probes = []
        for session in sessions:
            try:
                probes.extend(interesting(probe_lines(session)))
            except Exception:
                pass
        record["probeGenerations"] = probes
        write_json(step_dir / "evidence.json", record)
    return record, fatal


def source_identity(step: Mapping[str, Any], manifest: Mapping[str, Any], names: Mapping[str, Mapping[str, str]]) -> dict[str, Any]:
    if "sourceRepoId" in step:
        repo = repo_by_id(manifest, step["sourceRepoId"])
        return {
            "repoId": repo["repoId"],
            "displayName": repo["basename"],
            "relativePath": repo["relativePath"],
            "canonicalNote": "Duplicate basenames use workspace-relative display labels. Selection checks the canonical REPO_SELECTING root.",
            "operations": [
                {
                    "path": op.get("source", op.get("operation", {})).get("path") if isinstance(op, dict) else None,
                    "kind": (op.get("source") or {}).get("kind"),
                    "sourceKind": (op.get("source") or {}).get("sourceKind") or op.get("sourceKind"),
                    "oid": (op.get("source") or {}).get("oid"),
                    "rev": (op.get("source") or {}).get("rev"),
                    "op": op.get("op"),
                }
                for op in step.get("operations") or []
            ],
            "selection": {
                "oids": (step.get("selection") or {}).get("oids"),
                "tipRev": (step.get("selection") or {}).get("tipRev"),
                "baseRev": (step.get("selection") or {}).get("baseRev"),
            },
        }
    return {"reason": step.get("reason"), "detail": step.get("detail")}


def execute_step(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    step_dir: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    before: Mapping[str, Any],
) -> None:
    kind = step["kind"]
    if kind == "positive-file":
        run_positive_file(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, before)
        return
    if kind == "positive-commit":
        run_positive_commit(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, before)
        return
    if kind == "negative":
        run_negative(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, before)
        return
    raise DriverError(f"unsupported step kind {kind}")


def machine_session(sessions: Mapping[str, Any], repo: Mapping[str, Any]) -> Any:
    return sessions[machine_of(repo)]


def window_of(session: Any, timeout: float) -> dict[str, Any]:
    return session.window(timeout=timeout)


def run_positive_file(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    step_dir: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    before: Mapping[str, Any],
) -> None:
    source = repo_by_id(manifest, step["sourceRepoId"])
    dest = repo_by_id(manifest, step["destRepoId"])
    source_session = machine_session(sessions, source)
    dest_session = machine_session(sessions, dest)
    source_win = window_of(source_session, timeout)
    select_repo(native, source_session, source_win, source["basename"], fixture / source["relativePath"], timeout, trace)
    record["screenshots"]["graph"] = relative_shot(step_dir.parents[1], capture_checked(native, source_session, source_session.window(timeout=timeout), "graph", timeout))
    for op in step["operations"]:
        op_source = op.get("source") or {}
        kind = op_source.get("kind") or op.get("kind")
        if kind == "fixed-oid":
            select_fixed_oid(
                native,
                source_session,
                source_session.window(timeout=timeout),
                fixture / source["relativePath"],
                op_source["rev"],
                op_source["path"],
                op_source["oid"],
                timeout,
                trace,
            )
            continue
        ui_source, path = resolve_operation_source(op)
        leave_rev_tree_if_open(native, source_session, source_session.window(timeout=timeout), timeout, trace)
        preview_change(native, source_session, source_session.window(timeout=timeout), ui_source, path, timeout, trace, True)
    leave_rev_tree_if_open(native, source_session, source_session.window(timeout=timeout), timeout, trace)
    record["screenshots"]["source-selected"] = relative_shot(
        step_dir.parents[1], capture_checked(native, source_session, source_session.window(timeout=timeout), "source-selected", timeout)
    )
    payload = copy_from_button(native, source_session, source_session.window(timeout=timeout), timeout, trace)
    source_meta = save_payload(step_dir / "clipboard-source.bin", payload)
    dest_win = window_of(dest_session, timeout)
    select_repo(native, dest_session, dest_win, dest["basename"], fixture / dest["relativePath"], timeout, trace)
    bridged = transfer_os_clipboard(source_session, dest_session)
    readback = dest_session.read_clipboard()
    read_meta = save_payload(step_dir / "clipboard-readback.bin", readback)
    record["clipboard"] = {"source": source_meta, "readback": read_meta, "bridge": bridged}
    paste_cursor = len(dest_session.lines)
    paste_preview(native, dest_session, dest_session.window(timeout=timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(
        step_dir.parents[1], capture_checked(native, dest_session, dest_session.window(timeout=timeout), "preview", timeout)
    )
    resolve_paste_mappings(
        native,
        dest_session,
        dest_session.window(timeout=timeout),
        manifest,
        step["destRepoId"],
        fixture,
        timeout,
        trace,
        start_line=paste_cursor,
    )
    overwrite_paths = [op["dest"]["path"] for op in step["operations"] if op.get("overwrite")]
    click_overwrites(native, dest_session, dest_session.window(timeout=timeout), overwrite_paths, timeout, trace)
    line = apply_or_cancel(native, dest_session, dest_session.window(timeout=timeout), "btn-apply", timeout, trace)
    if "PASTE_DONE" not in line:
        raise UiRefusal(line)
    record["screenshots"]["result"] = relative_shot(
        step_dir.parents[1], capture_checked(native, dest_session, dest_session.window(timeout=timeout), "result", timeout)
    )
    compare_after(fixture, step, step_dir, record, before)


def run_positive_commit(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    step_dir: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    before: Mapping[str, Any],
) -> None:
    source = repo_by_id(manifest, step["sourceRepoId"])
    dest = repo_by_id(manifest, step["destRepoId"])
    source_session = machine_session(sessions, source)
    dest_session = machine_session(sessions, dest)
    select_repo(
        native,
        source_session,
        window_of(source_session, timeout),
        source["basename"],
        fixture / source["relativePath"],
        timeout,
        trace,
    )
    record["screenshots"]["graph"] = relative_shot(
        step_dir.parents[1], capture_checked(native, source_session, source_session.window(timeout=timeout), "graph", timeout)
    )
    select_commit_range(
        native,
        source_session,
        source_session.window(timeout=timeout),
        fixture / source["relativePath"],
        step["selection"],
        timeout,
        trace,
    )
    record["screenshots"]["source-selected"] = relative_shot(
        step_dir.parents[1], capture_checked(native, source_session, source_session.window(timeout=timeout), "source-selected", timeout)
    )
    payload, line = copy_commits(native, source_session, source_session.window(timeout=timeout), timeout, trace)
    record["source"]["copyLine"] = line
    record["source"]["fullOids"] = list(step["selection"]["oids"])
    source_meta = save_payload(step_dir / "clipboard-source.bin", payload)
    select_repo(
        native,
        dest_session,
        window_of(dest_session, timeout),
        dest["basename"],
        fixture / dest["relativePath"],
        timeout,
        trace,
    )
    bridged = transfer_os_clipboard(source_session, dest_session)
    readback = dest_session.read_clipboard()
    read_meta = save_payload(step_dir / "clipboard-readback.bin", readback)
    record["clipboard"] = {"source": source_meta, "readback": read_meta, "bridge": bridged}
    paste_preview(native, dest_session, dest_session.window(timeout=timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(
        step_dir.parents[1], capture_checked(native, dest_session, dest_session.window(timeout=timeout), "preview", timeout)
    )
    click_overwrites(native, dest_session, dest_session.window(timeout=timeout), None, timeout, trace)
    done = apply_or_cancel(native, dest_session, dest_session.window(timeout=timeout), "btn-apply", timeout, trace)
    if "PASTE_DONE" not in done:
        raise UiRefusal(done)
    record["screenshots"]["result"] = relative_shot(
        step_dir.parents[1], capture_checked(native, dest_session, dest_session.window(timeout=timeout), "result", timeout)
    )
    compare_after(fixture, step, step_dir, record, before)


def compare_after(fixture: Path, step: Mapping[str, Any], step_dir: Path, record: dict[str, Any], before: Mapping[str, Any]) -> None:
    after_path = step_dir / "after.json"
    after = snapshot_file(fixture, after_path)
    record["snapshots"]["after"] = {"path": str(after_path), "sha256": snapshot_digest(after_path)}
    try:
        compare_step(str(fixture), step["id"], after, "applied")
    except CompareError as exc:
        raise DriverError(f"compare_step applied failed: {exc}") from exc
    record["compare"]["applied"] = "ok"
    if step["kind"] == "positive-commit":
        record["compare"]["replayMetadataFromGitObjects"] = new_commit_metadata(before, after, step["destRepoId"])


def run_negative(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    step_dir: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    before: Mapping[str, Any],
) -> None:
    reason = step["reason"]
    output = step_dir.parents[1]
    if reason == "cross-repository":
        block_cross_repo(native, sessions, manifest, step, names, fixture, timeout, trace, record, output, step_dir=step_dir)
    elif reason == "discontinuous":
        refuse_discontinuous(native, sessions, manifest, step, names, fixture, timeout, trace, record, output)
    elif reason == "target-collision":
        attempt_collision(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, output)
    elif reason == "ambiguous-basename":
        block_ambiguous(native, sessions, manifest, names, fixture, step_dir, timeout, trace, record, output, step=step)
    elif reason == "missing-destination":
        block_missing_dest(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, output)
    elif reason == "stale-source":
        run_stale_source(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, output)
    elif reason == "stale-target":
        run_stale_target(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, output)
    elif reason == "overwrite-unauthorized":
        run_unauthorized(native, sessions, manifest, step, names, fixture, step_dir, timeout, trace, record, output)
    elif reason == "cancel":
        run_cancel(native, sessions, manifest, names, fixture, step_dir, timeout, trace, record, output)
    else:
        raise MissingControl(reason, "negative reason has no real control sequence")
    assert_negative_unchanged(fixture, step, step_dir, record, before)


def try_apply_refusal(native: Any, session: Any, win: dict[str, Any], timeout: float, trace: list[dict[str, Any]], start_line: int = 0) -> str:
    """Attempt to apply paste; in negative scenarios, assert that Apply is ignored or refused.

    1. Checks fresh plan mapping=false from [APP:PASTE_PREVIEW: ... mapping=false].
    2. Sends keyboard Return (bound to ApplyPaste/enter in PastePanel) and observes
       rejection [APP:PASTE_ERR: mapping_required].
    3. Clicks disabled btn-apply (renders disabled with no click listener).
    4. Bounded observation: asserts no [APP:PASTE_APPLYING] and no [APP:PASTE_DONE:].
    5. Fails closed: unexpected applying/done or missing control always raises.
       Never treats timeout or absent event as success proof.
    """
    recent_texts = session.texts(start_line)
    mapping_false_seen = any("mapping=false" in line for line in recent_texts if "PASTE_PREVIEW:" in line)
    if not mapping_false_seen:
        raise MissingControl(
            "apply-refusal",
            "fresh PASTE_PREVIEW with mapping=false was not observed before apply refusal attempt",
        )

    # Attempt keyboard enter/Return (GPUI keybinding 'enter' for ApplyPaste in PastePanel)
    key_before = len(session.lines)
    session.key(win["wid"], "Return")
    try:
        line = wait_any(
            session,
            ("[APP:PASTE_ERR:", "[APP:PASTE_APPLYING]", "[APP:PASTE_DONE:"),
            key_before,
            timeout,
        )
    except UiRefusal as exc:
        if "[APP:PASTE_ERR: mapping_required]" not in exc.line:
            raise
        line = exc.line
    if "PASTE_APPLYING" in line or "PASTE_DONE" in line:
        raise DriverError(f"paste unexpectedly applied on keyboard action: {line}")
    if "PASTE_ERR: mapping_required" not in line:
        raise DriverError(f"expected [APP:PASTE_ERR: mapping_required] on Enter, got: {line}")
    keyboard_refusal = line
    trace.append({"action": "try-apply-keyboard", "line": line})

    # Real click on rendered disabled btn-apply
    click_control(native, session, win, "btn-apply", timeout)

    # Bounded observation: verify no applying or done lines appeared after click
    deadline = time.monotonic() + min(timeout, 1.0)
    while True:
        click_texts = session.texts(key_before)
        if any("PASTE_APPLYING" in l or "PASTE_DONE" in l for l in click_texts):
            raise DriverError("paste apply executed after clicking disabled btn-apply")
        if session.proc.poll() is not None:
            raise DriverError("app exited while observing disabled btn-apply")
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)

    trace.append({"action": "try-apply-refusal", "line": keyboard_refusal, "clicked": True})
    return keyboard_refusal


def assert_negative_unchanged(fixture: Path, step: Mapping[str, Any], step_dir: Path, record: dict[str, Any], before: Mapping[str, Any]) -> None:
    after_path = step_dir / "after.json"
    after = snapshot_file(fixture, after_path)
    record["snapshots"]["after"] = {"path": str(after_path), "sha256": snapshot_digest(after_path)}
    pre_path = record["snapshots"].get("preAction", {}).get("path")
    if step.get("noWriteSnapshot") == "immediately-before-action-after-setup-diff":
        if not pre_path:
            raise DriverError("stale step is missing the pre-action snapshot")
        pre = json.loads(Path(pre_path).read_text(encoding="utf-8"))
        if not repos_equal(pre, after):
            raise DriverError("action changed repos relative to the pre-action snapshot")
        record["compare"]["applied"] = "equals-pre-action-snapshot"
        return
    if not repos_equal(before, after):
        raise DriverError("negative step changed the baseline")
    try:
        compare_step(str(fixture), step["id"], after, "applied")
    except CompareError as exc:
        raise DriverError(f"compare_step applied failed: {exc}") from exc
    record["compare"]["applied"] = "equals-baseline"


def parse_wire_commit_payload(payload: bytes) -> dict[str, Any]:
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise DriverError(f"wire commit payload is not valid UTF-8: {exc}") from exc
    first, _, rest = text.partition("\n")
    if first.rstrip("\r") != "// snip-sync commits v1":
        raise DriverError(f"wire commit payload lacks expected header '// snip-sync commits v1', got: {first[:60]!r}")
    try:
        data = json.loads(rest)
    except Exception as exc:
        raise DriverError(f"wire commit payload body is not valid JSON: {exc}") from exc
    if not isinstance(data, dict) or "commits" not in data or not isinstance(data["commits"], list):
        raise DriverError(f"wire commit payload does not contain commits array: {data}")
    return data


def commit_payload_oracle(repo: Path, oids: Sequence[str], timeout: float) -> dict[str, Any]:
    """Read the selected Git objects independently; the wire has no commit OID field."""
    commits = []
    for oid in oids:
        meta = git_read_bytes(repo, ["log", "-1", "-z", "--format=%an%x00%ae%x00%aI%x00%B", oid], timeout)
        name, email, date, message = meta.removesuffix(b"\0").decode("utf-8").split("\0", 3)
        parents = git_read(repo, ["rev-list", "--parents", "-n", "1", oid], timeout).split()[1:]
        raw = git_read_bytes(repo, ["diff-tree", "-r", "-z", "--raw", "--no-abbrev", "--no-commit-id", "-M", parents[0] if parents else EMPTY_TREE, oid], timeout)
        files = []
        for entry in parse_diff_tree_z(raw):
            status = entry["status"][0]
            change = {"A": "ADDED", "C": "ADDED", "D": "DELETED", "R": "RENAMED"}.get(status, "MODIFIED")
            file = {"path": entry["path"], "oldPath": entry["oldPath"] or None, "change": change, "content": None, "notCopied": None}
            modes = [entry["oldMode"]] if status == "D" else [entry["newMode"]]
            if status == "R":
                modes.append(entry["oldMode"])
            if any(mode in ("120000", "160000") for mode in modes):
                file["notCopied"] = "UNSUPPORTED_TYPE"
            elif status != "D":
                blob = git_read_bytes(repo, ["cat-file", "blob", entry["newSha"]], timeout)
                if b"\0" in blob:
                    file["notCopied"] = "BINARY"
                else:
                    try:
                        file["content"] = blob.decode("utf-8")
                    except UnicodeDecodeError:
                        file["notCopied"] = "NON_UTF8"
            files.append(file)
        commits.append({"message": message, "authorName": name, "authorEmail": email, "authorDate": date, "files": files})
    return {"commits": commits}


def require_exact_commit_payload(payload: bytes, expected: Mapping[str, Any]) -> None:
    if parse_wire_commit_payload(payload) != expected:
        raise DriverError("exported commit payload differs from the exact Git oracle (sequence, metadata, or files)")


def block_cross_repo(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    step: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    output: Path,
    step_dir: Path | None = None,
) -> None:
    first = repo_by_id(manifest, step["tips"][0]["repoId"])
    second = repo_by_id(manifest, step["tips"][1]["repoId"])
    session = machine_session(sessions, first)
    win = window_of(session, timeout)
    first_repo_dir = fixture / first["relativePath"]
    second_repo_dir = fixture / second["relativePath"]

    # Step 1: Select first repo and select tip 1
    select_repo(native, session, win, first["basename"], first_repo_dir, timeout, trace)
    tip1 = git_read(first_repo_dir, ["rev-parse", "--verify", step["tips"][0]["rev"]], timeout).strip()
    click_commit_row(native, session, win, first_repo_dir, tip1, timeout, trace)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, session, win, "source-selected", timeout))

    # Step 2: Switch to second repo
    select_repo(native, session, win, second["basename"], second_repo_dir, timeout, trace)

    # Step 3: Select tip 2 in second repo
    tip2 = git_read(second_repo_dir, ["rev-parse", "--verify", step["tips"][1]["rev"]], timeout).strip()
    click_commit_row(native, session, win, second_repo_dir, tip2, timeout, trace)

    # Step 4: Copy commits from second repo
    payload, copy_line = copy_commits(native, session, win, timeout, trace)
    payload_sha = sha256_bytes(payload)

    target_dir = step_dir if step_dir is not None else output
    source_meta = save_payload(target_dir / "clipboard-source.bin", payload)

    oracle = commit_payload_oracle(second_repo_dir, [tip2], timeout)
    oracle_path = target_dir / "commit-oracle.json"
    write_json(oracle_path, {"repo": str(second_repo_dir), "oids": [tip2], "payload": oracle})
    require_exact_commit_payload(payload, oracle)

    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, session, win, "result", timeout))
    record["clipboard"] = {
        "sourceSha256": payload_sha,
        "source": source_meta,
        "exactPayloadOracle": True,
        "crossRepoPrevented": True,
        "firstRepoTip": tip1,
        "secondRepoTip": tip2,
        "commitCount": len(oracle["commits"]),
        "oracle": {"path": str(oracle_path), "sha256": sha256_file(oracle_path)},
    }
    record["status"] = "passed"
    record["compare"]["applied"] = "equals-baseline"
    record["compare"]["prevention"] = "cross-repo commit selection prevented by repository isolation; exported payload matches second repo exactly"


def refuse_discontinuous(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    repo = repo_by_id(manifest, "a-west-billing")
    session = sessions["a"]
    sentinel = f"SNIP-DISCONTINUOUS-SENTINEL-{uuid.uuid4().hex}\n".encode("utf-8")
    session.set_clipboard(sentinel)
    sentinel_sha = sha256_bytes(sentinel)

    select_repo(native, session, window_of(session, timeout), repo["basename"], fixture / repo["relativePath"], timeout, trace)
    selection = step["selection"]
    oids = [selection["baseOid"], selection["tipOid"]]
    record["screenshots"]["graph"] = relative_shot(output, capture_checked(native, session, window_of(session, timeout), "graph", timeout))
    click_commit_row(native, session, window_of(session, timeout), fixture / repo["relativePath"], oids[0], timeout, trace)
    before = len(session.lines)
    shift_click_commit(native, session, window_of(session, timeout), fixture / repo["relativePath"], oids[-1], timeout, trace)
    ranged = wait_substr(session, "[APP:RANGE: commits=", before, timeout)
    trace.append({"action": "discontinuous-range", "line": ranged})
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, session, window_of(session, timeout), "source-selected", timeout))
    try:
        copy_commits(native, session, window_of(session, timeout), timeout, trace, sentinel=sentinel)
    except UiRefusal as exc:
        if "[APP:COPY_COMMITS_ERR: commits are not contiguous:" not in exc.line:
            raise DriverError(f"refusal does not prove discontinuous selection: {exc.line}") from exc
        current_cb = session.read_clipboard()
        if current_cb != sentinel:
            raise DriverError(f"clipboard was modified despite refusal: {current_cb!r}")
        record["clipboard"] = {
            "sentinelSha256": sentinel_sha,
            "clipboardMatchesSentinel": True,
            "refused": exc.line,
        }
        record["screenshots"]["result"] = relative_shot(output, capture_checked(native, session, window_of(session, timeout), "result", timeout))
        record["status"] = "passed"
        record["compare"]["refusal"] = exc.line
        return
    raise DriverError(f"discontinuous tips copied after {ranged.strip()}; payload was not pasted")


def select_tree_file(native: Any, session: Any, repo: Mapping[str, Any], fixture: Path, path: str, timeout: float, trace: list[dict[str, Any]]) -> None:
    win = window_of(session, timeout)
    select_repo(native, session, win, repo["basename"], fixture / repo["relativePath"], timeout, trace)
    native.open_project_list(session, win)
    for parent in reversed(Path(path).parents):
        if str(parent) == ".":
            continue
        before = len(session.lines)
        # The chevron opens a folder without selecting it (a row click would
        # select the folder alone and drop the other repos' picks).
        click_control(native, session, win, f"tree-chevron:{parent}", timeout)
        wait_substr(session, f"[APP:TREE_EXPANDED: {parent}]", before, timeout)
    before = len(session.lines)
    click_control(native, session, win, f"tree-row:{path}", timeout, modifier="ctrl")
    wait_substr(session, "[APP:BASKET: n=", before, timeout)
    trace.append({"action": "tree-check", "repo": repo["repoId"], "path": path})


def attempt_collision(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    maps = step["maps"]
    source_session = sessions["a"]
    for item in maps:
        repo = repo_by_id(manifest, item["sourceRepoId"])
        select_tree_file(native, source_session, repo, fixture, item["sourcePath"], timeout, trace)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, source_session, window_of(source_session, timeout), "source-selected", timeout))

    payload = copy_from_button(native, source_session, window_of(source_session, timeout), timeout, trace)
    source_meta = save_payload(step_dir / "clipboard-source.bin", payload)

    dest = repo_by_id(manifest, maps[0]["destRepoId"])
    dest_session = sessions["b"]
    dest_canonical_root = str(os.path.realpath(fixture / dest["relativePath"]))
    select_repo(native, dest_session, window_of(dest_session, timeout), dest["basename"], fixture / dest["relativePath"], timeout, trace)
    transfer_os_clipboard(source_session, dest_session)

    gen_before = len(dest_session.lines)
    paste_preview(native, dest_session, window_of(dest_session, timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "preview", timeout))

    # Parse candidates from current generation texts
    current_lines = dest_session.texts(gen_before)
    prefix_candidates = parse_paste_map_candidates(current_lines)

    # The active selected root is primary: its repo-relative path has no repo prefix.
    # Thus the second selected src/app.txt uses src->keep-relative, not docs->dest.
    primary_repo_id = maps[-1]["sourceRepoId"]
    choices = []
    exported_paths = []
    destination_paths = []
    for item in maps:
        source_repo = repo_by_id(manifest, item["sourceRepoId"])
        keep = item["sourceRepoId"] == primary_repo_id
        prefix = item["sourcePath"].split("/", 1)[0] if keep else source_repo["basename"]
        choices.append((prefix, keep))
        exported_paths.append(item["sourcePath"] if keep else f"{prefix}/{item['sourcePath']}")
        if item["destRepoId"] != dest["repoId"] or item["sourcePath"] != item["destPath"]:
            raise DriverError("collision mappings must preserve the same repo-relative destination path")
        destination_paths.append(str(Path(dest_canonical_root) / item["destPath"]))
    headers = re.findall(r"^// file: (.+)$", payload.decode("utf-8"), re.MULTILINE)
    if Counter(headers) != Counter(exported_paths):
        raise DriverError(f"collision export paths differ from selected source oracle: {headers} != {exported_paths}")
    if len(choices) != 2 or len({prefix for prefix, _ in choices}) != 2 or len(set(destination_paths)) != 1:
        raise DriverError("collision requires two distinct source prefixes with one exact canonical destination file")
    mapped_prefixes = []
    final_pick_cursor = gen_before
    for prefix, keep in choices:
        candidates = prefix_candidates.get(prefix, [])
        matching = [idx for idx, path in candidates if os.path.realpath(path) == dest_canonical_root]
        if len(matching) != 1:
            raise MissingControl(f"paste-map-pick:{prefix}", f"no unique canonical destination {dest_canonical_root}: {candidates}")
        control = f"paste-map-keep:{prefix}" if keep else f"paste-map-pick:{prefix}:{matching[0]}"
        final_pick_cursor = len(dest_session.lines)
        click_control(native, dest_session, window_of(dest_session, timeout), control, timeout, viewport="paste-items")
        line = wait_any(dest_session, ("[APP:PASTE_MAPPED:", "[APP:PASTE_ERR:", "[APP:PASTE_DONE:", "[APP:PASTE_APPLYING]"), final_pick_cursor, timeout)
        match = PASTE_MAPPED_RE.search(line)
        if not match or match["prefix"] != prefix or os.path.realpath(match["dest"]) != dest_canonical_root or match["keep"] != ("primary" if keep else None):
            raise DriverError(f"collision mapping was not confirmed for {prefix}: {line}")
        mapped_prefixes.append(prefix)
        trace.append({"action": "paste-map-collision", "control": control, "dest": dest_canonical_root, "keepRelative": keep, "line": line})

    # The worker emits this before PASTE_MAPPED when the second replan collides.
    refusal_reason = wait_substr(dest_session, "[APP:PASTE_PLAN_REFUSED: reason=target_collision]", final_pick_cursor, timeout)
    if any("PASTE_APPLYING" in line or "PASTE_DONE" in line for line in dest_session.texts(gen_before)):
        raise DriverError("colliding operations were applied")

    # Cancel dialog to leave UI in clean state
    apply_or_cancel(native, dest_session, window_of(dest_session, timeout), "btn-cancel", timeout, trace)
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "result", timeout))
    record["clipboard"] = {"sourceSha256": source_meta["sha256"], "refusal": refusal_reason, "collisionMapped": mapped_prefixes, "exportedPaths": exported_paths, "canonicalDestinationFiles": destination_paths}
    record["status"] = "passed"
    record["compare"]["applied"] = "equals-baseline"
    record["compare"]["refusal"] = refusal_reason


def block_ambiguous(
    native: Any,
    sessions: Mapping[str, Any],
    manifest: Mapping[str, Any],
    names: Mapping[str, Mapping[str, str]],
    fixture: Path,
    step_dir: Path,
    timeout: float,
    trace: list[dict[str, Any]],
    record: dict[str, Any],
    output: Path,
    step: Mapping[str, Any] | None = None,
) -> None:
    # Manifest candidateRepoIds are "a-west-billing" and "a-east-billing", both in workspace A.
    # To require prefix mapping for billing, construct a multi-root export via REAL UI:
    # Select file in a-west-billing and select file in a-west-docs.
    billing_repo = repo_by_id(manifest, "a-west-billing")
    docs_repo = repo_by_id(manifest, "a-west-docs")
    session_a = sessions["a"]
    win_a = window_of(session_a, timeout)

    select_tree_file(native, session_a, billing_repo, fixture, "src/app.txt", timeout, trace)
    select_tree_file(native, session_a, docs_repo, fixture, "src/app.txt", timeout, trace)

    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, session_a, win_a, "source-selected", timeout))

    # 3. Copy multi-root bundle via UI
    payload = copy_from_button(native, session_a, win_a, timeout, trace)
    source_meta = save_payload(step_dir / "clipboard-source.bin", payload)

    # 4. Paste within workspace A (which contains both a-west-billing and a-east-billing)
    dest_repo = billing_repo
    select_repo(native, session_a, win_a, dest_repo["basename"], fixture / dest_repo["relativePath"], timeout, trace)

    paste_cursor = len(session_a.lines)
    paste_preview(native, session_a, win_a, timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, session_a, win_a, "preview", timeout))

    # Assert preview logged mapping=false (because billing is ambiguous in workspace A)
    preview_texts = session_a.texts(paste_cursor)
    mapping_false_seen = any("mapping=false" in line for line in preview_texts if "PASTE_PREVIEW:" in line)
    if not mapping_false_seen:
        raise DriverError("expected PASTE_PREVIEW with mapping=false for ambiguous billing mapping")

    # 5. Extract candidate paths for 'billing' from current generation texts
    prefix_candidates = parse_paste_map_candidates(preview_texts)
    billing_candidates = prefix_candidates.get("billing", [])
    if len(billing_candidates) < 2:
        raise DriverError(f"expected at least 2 ambiguous candidates for 'billing', found {billing_candidates}")

    # Assert candidate paths match manifest candidateRepoIds: a-west-billing and a-east-billing
    expected_roots = {
        str(os.path.realpath(fixture / repo_by_id(manifest, rid)["relativePath"]))
        for rid in (step or {}).get("candidateRepoIds", ("a-west-billing", "a-east-billing"))
    }
    candidate_roots = {str(os.path.realpath(path)) for _, path in billing_candidates}
    if not expected_roots.issubset(candidate_roots):
        raise DriverError(f"candidate roots {candidate_roots} do not contain expected manifest roots {expected_roots}")

    # 6. Attempt Apply refusal without resolving ambiguous mapping
    apply_line = try_apply_refusal(native, session_a, win_a, timeout, trace, start_line=paste_cursor)
    if "PASTE_DONE" in apply_line:
        raise DriverError(f"unresolved ambiguous mapping applied instead of refused: {apply_line}")

    # 7. Cancel dialog
    apply_or_cancel(native, session_a, win_a, "btn-cancel", timeout, trace)
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, session_a, win_a, "result", timeout))
    record["clipboard"] = {
        "sourceSha256": source_meta["sha256"],
        "refusal": apply_line,
        "candidateCount": len(billing_candidates),
        "candidates": [path for _, path in billing_candidates],
    }
    record["status"] = "passed"
    record["compare"]["applied"] = "equals-baseline"
    record["compare"]["refusal"] = apply_line


def block_missing_dest(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    # Construct multi-root export on session A so prefix mapping is required
    billing_repo = repo_by_id(manifest, "a-west-billing")
    docs_repo = repo_by_id(manifest, "a-west-docs")
    source_session = sessions["a"]
    win_a = window_of(source_session, timeout)

    if step["destRepoId"] in {repo["repoId"] for repo in manifest["repos"]}:
        raise DriverError("missing-destination fixture names an existing repository")
    if step["sourceRepoId"] != billing_repo["repoId"]:
        raise DriverError("missing-destination source does not match selected repository")
    select_tree_file(native, source_session, billing_repo, fixture, step["sourcePath"], timeout, trace)
    select_tree_file(native, source_session, docs_repo, fixture, "src/app.txt", timeout, trace)

    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, source_session, win_a, "source-selected", timeout))
    payload = copy_from_button(native, source_session, win_a, timeout, trace)
    source_meta = save_payload(step_dir / "clipboard-source.bin", payload)

    # Destination on session b: b-north-ledger
    dest_probe = repo_by_id(manifest, "b-north-ledger")
    dest_session = sessions["b"]
    win_b = window_of(dest_session, timeout)
    select_repo(native, dest_session, win_b, dest_probe["basename"], fixture / dest_probe["relativePath"], timeout, trace)
    transfer_os_clipboard(source_session, dest_session)

    paste_cursor = len(dest_session.lines)
    paste_preview(native, dest_session, win_b, timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, dest_session, win_b, "preview", timeout))

    # Record the real unresolved-prefix state without claiming the missing ID was submitted.
    current_lines = dest_session.texts(paste_cursor)
    prefix_candidates = parse_paste_map_candidates(current_lines)
    prefix = billing_repo["basename"]
    candidates = prefix_candidates.get(prefix, [])
    expected_roots = {os.path.realpath(fixture / repo["relativePath"]) for repo in manifest["repos"] if machine_of(repo) == "b"}
    if len(expected_roots) != 15 or not all(Path(root).is_dir() for root in expected_roots):
        raise DriverError("whitelist oracle requires all 15 existing machine B repository roots")
    candidate_roots = {os.path.realpath(path) for _, path in candidates}
    if not candidates or candidate_roots != expected_roots:
        raise DriverError(f"unresolved {prefix} candidates do not match workspace B canonical roots: {candidate_roots}")
    if any(PASTE_MAPPED_RE.search(line) for line in current_lines):
        raise DriverError("missing-destination prefix was unexpectedly mapped")

    # Verify preview logged mapping=false
    mapping_false_seen = any("mapping=false" in line for line in current_lines if "PASTE_PREVIEW:" in line)
    if not mapping_false_seen:
        raise DriverError("expected PASTE_PREVIEW with mapping=false for missing destination")

    # Attempt Apply without resolving mapping
    apply_line = try_apply_refusal(native, dest_session, win_b, timeout, trace, start_line=paste_cursor)
    if "PASTE_DONE" in apply_line:
        raise DriverError(f"missing destination plan applied instead of refused: {apply_line}")

    # Cancel dialog
    apply_or_cancel(native, dest_session, win_b, "btn-cancel", timeout, trace)
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, dest_session, win_b, "result", timeout))
    readback = dest_session.read_clipboard()
    if readback != payload:
        raise ClipboardMismatch("destination whitelist refusal changed the clipboard")
    record["clipboard"] = {
        "sourceSha256": source_meta["sha256"],
        "readbackSha256": sha256_bytes(readback),
        "clipboardMatchesSource": True,
        "prevention": "canonical-destination-whitelist",
        "originalMappedIdSubmitted": False,
        "automaticMappingObserved": False,
        "disabledApplyClicked": True,
        "unresolvedPrefix": prefix,
        "candidateCanonicalRoots": sorted(candidate_roots),
        "workspaceCanonicalRoots": sorted(expected_roots),
        "candidateRepoIds": sorted(repo["repoId"] for repo in manifest["repos"] if machine_of(repo) == "b"),
        "missingDestRepoId": step["destRepoId"],
        "refusal": apply_line,
    }
    if not valid_whitelist_prevention(record["clipboard"]):
        raise DriverError("incomplete destination whitelist prevention evidence")
    record["compare"]["prevention"] = "canonical destination whitelist prevents submitting the nonexistent mapped ID; originalMappedIdSubmitted=false"


def run_stale_source(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    repo = repo_by_id(manifest, step["preview"]["repoId"])
    session = sessions["a"]
    select_repo(native, session, window_of(session, timeout), repo["basename"], fixture / repo["relativePath"], timeout, trace)
    preview_change(native, session, window_of(session, timeout), "working", step["preview"]["path"], timeout, trace, True)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, session, window_of(session, timeout), "source-selected", timeout))
    hold = step_dir / "export_hold.signal"
    sentinel = f"SNIP-STALE-SOURCE-SENTINEL-{uuid.uuid4().hex}\n".encode()
    session.set_clipboard(sentinel)
    if session.read_clipboard() != sentinel:
        raise ClipboardMismatch("stale-source sentinel did not stick")
    before_copy = len(session.lines)
    hold.write_text("hold export before final revalidation\n", encoding="utf-8")
    try:
        click_control(native, session, window_of(session, timeout), "btn-copy", timeout)
        ready = wait_any(session, ("[APP:EXPORT_PLAN_READY:", "[APP:COPY_DONE:", "[APP:COPY_FAILED:", "[APP:COPY_REFUSED:"), before_copy, timeout)
        if not re.search(r"\[APP:EXPORT_PLAN_READY: files=[1-9][0-9]*\]", ready):
            raise DriverError(f"export did not stop after capturing a nonempty plan: {ready}")
        trace.append({"action": "export-plan-held", "line": ready})
        mutated = mutate_owned_file(fixture, repo["relativePath"], step["preview"]["path"], b"\nSTALE-SOURCE\n")
        write_json(step_dir / "setup.diff", mutated)
        pre_path = step_dir / "pre-action.json"
        snapshot_file(fixture, pre_path)
        record["snapshots"]["preAction"] = {"path": str(pre_path), "sha256": snapshot_digest(pre_path)}
        record["snapshots"]["setup"] = mutated
        before_release = len(session.lines)
    finally:
        hold.unlink(missing_ok=True)
    failure = wait_any(session, ("[APP:COPY_FAILED:", "[APP:COPY_DONE:", "[APP:COPY_REFUSED:"), before_release, timeout)
    if "[APP:COPY_FAILED: stale_source]" not in failure:
        raise DriverError(f"export did not refuse stale source: {failure}")
    idle = wait_substr(session, "[APP:COPY_IDLE]", before_release, timeout)
    if any("[APP:COPY_DONE:" in line for line in session.texts(before_copy)):
        raise DriverError("stale-source export published a payload")
    if session.read_clipboard() != sentinel:
        raise ClipboardMismatch("stale-source refusal changed the clipboard sentinel")
    record["clipboard"] = {"sentinelSha256": sha256_bytes(sentinel), "clipboardMatchesSentinel": True, "refused": failure}
    trace.append({"action": "stale-source-refused", "line": failure, "idle": idle})
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, session, window_of(session, timeout), "result", timeout))
    record["compare"]["freshness"] = "plan captured, source mutated, final revalidation refused"


def run_stale_target(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    source = repo_by_id(manifest, "a-west-billing")
    dest = repo_by_id(manifest, step["preview"]["repoId"])
    path = step["preview"]["path"]
    source_session = sessions["a"]
    dest_session = sessions["b"]
    select_repo(native, source_session, window_of(source_session, timeout), source["basename"], fixture / source["relativePath"], timeout, trace)
    native.open_project_list(source_session, window_of(source_session, timeout))
    parent = path.rsplit("/", 1)[0]
    if parent:
        before = len(source_session.lines)
        click_control(native, source_session, window_of(source_session, timeout), f"tree-chevron:{parent}", timeout)
        wait_substr(source_session, f"[APP:TREE_EXPANDED: {parent}]", before, timeout)
    before = len(source_session.lines)
    click_control(native, source_session, window_of(source_session, timeout), f"tree-row:{path}", timeout, modifier="ctrl")
    wait_substr(source_session, "[APP:BASKET: n=", before, timeout)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, source_session, window_of(source_session, timeout), "source-selected", timeout))
    payload = copy_from_button(native, source_session, window_of(source_session, timeout), timeout, trace)
    save_payload(step_dir / "clipboard-source.bin", payload)
    select_repo(native, dest_session, window_of(dest_session, timeout), dest["basename"], fixture / dest["relativePath"], timeout, trace)
    transfer_os_clipboard(source_session, dest_session)
    paste_preview(native, dest_session, window_of(dest_session, timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "preview", timeout))
    click_overwrites(native, dest_session, window_of(dest_session, timeout), [path], timeout, trace)
    mutated = mutate_owned_file(fixture, dest["relativePath"], path, b"\nSTALE-TARGET\n")
    (step_dir / "setup.diff").write_text(json.dumps(mutated, indent=2) + "\n", encoding="utf-8")
    pre_path = step_dir / "pre-action.json"
    snapshot_file(fixture, pre_path)
    record["snapshots"]["preAction"] = {"path": str(pre_path), "sha256": snapshot_digest(pre_path)}
    line = apply_or_cancel(native, dest_session, window_of(dest_session, timeout), "btn-apply", timeout, trace)
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "result", timeout))
    record["clipboard"] = {"sourceSha256": sha256_bytes(payload), "apply": line}
    if "PASTE_STALE_DETECTED:" not in line:
        raise DriverError(f"Apply did not log PASTE_STALE_DETECTED: {line}")
    record["status"] = "passed"
    record["compare"]["refusal"] = line


def run_unauthorized(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], step: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    op = step["operation"]
    source = repo_by_id(manifest, op["sourceRepoId"])
    dest = repo_by_id(manifest, op["destRepoId"])
    path = op["sourcePath"]
    source_session = sessions["a"]
    dest_session = sessions["b"]
    select_repo(native, source_session, window_of(source_session, timeout), source["basename"], fixture / source["relativePath"], timeout, trace)
    select_tree_file(native, source_session, source, fixture, path, timeout, trace)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, source_session, window_of(source_session, timeout), "source-selected", timeout))
    payload = copy_from_button(native, source_session, window_of(source_session, timeout), timeout, trace)
    save_payload(step_dir / "clipboard-source.bin", payload)
    select_repo(native, dest_session, window_of(dest_session, timeout), dest["basename"], fixture / dest["relativePath"], timeout, trace)
    transfer_os_clipboard(source_session, dest_session)
    preview = paste_preview(native, dest_session, window_of(dest_session, timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "preview", timeout))
    try:
        wait_control(native, dest_session, f"paste-overwrite:{path}", timeout)
    except Exception as exc:
        raise MissingControl(f"paste-overwrite:{path}", "unauthorized overwrite row was not rendered, so the refusal cannot be distinguished from a missing plan") from exc
    line = apply_or_cancel(native, dest_session, window_of(dest_session, timeout), "btn-apply", timeout, trace)
    record["clipboard"] = {"sourceSha256": sha256_bytes(payload), "preview": preview, "apply": line}
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "result", timeout))
    if "PASTE_DONE:" not in line or "overwritten=0" not in line:
        raise DriverError(f"unauthorized apply did not skip the write: {line}")
    record["status"] = "passed"
    record["compare"]["refusal"] = "overwrite left off; " + line


def run_cancel(native: Any, sessions: Mapping[str, Any], manifest: Mapping[str, Any], names: Mapping[str, Mapping[str, str]], fixture: Path, step_dir: Path, timeout: float, trace: list[dict[str, Any]], record: dict[str, Any], output: Path) -> None:
    source = repo_by_id(manifest, "a-west-billing")
    dest = repo_by_id(manifest, "b-north-ledger")
    source_session = sessions["a"]
    dest_session = sessions["b"]
    select_repo(native, source_session, window_of(source_session, timeout), source["basename"], fixture / source["relativePath"], timeout, trace)
    preview_change(native, source_session, window_of(source_session, timeout), "working", "transfer/working.txt", timeout, trace, True)
    record["screenshots"]["source-selected"] = relative_shot(output, capture_checked(native, source_session, window_of(source_session, timeout), "source-selected", timeout))
    payload = copy_from_button(native, source_session, window_of(source_session, timeout), timeout, trace)
    save_payload(step_dir / "clipboard-source.bin", payload)
    select_repo(native, dest_session, window_of(dest_session, timeout), dest["basename"], fixture / dest["relativePath"], timeout, trace)
    transfer_os_clipboard(source_session, dest_session)
    paste_preview(native, dest_session, window_of(dest_session, timeout), timeout, trace)
    record["screenshots"]["preview"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "preview", timeout))
    line = apply_or_cancel(native, dest_session, window_of(dest_session, timeout), "btn-cancel", timeout, trace)
    record["clipboard"] = {"sourceSha256": sha256_bytes(payload), "cancel": line}
    record["screenshots"]["result"] = relative_shot(output, capture_checked(native, dest_session, window_of(dest_session, timeout), "result", timeout))
    if "PASTE_CANCELLED" not in line:
        raise DriverError(f"cancel did not log PASTE_CANCELLED: {line}")
    record["status"] = "passed"


def render_markdown(report: Mapping[str, Any]) -> str:
    counts = report.get("counts") or {}
    lines = [
        "# Native collaboration UI report",
        "",
        f"Scope: {report.get('scope', 'pilot')}",
        "",
        f"- acceptanceComplete: {report.get('acceptanceComplete')}",
        f"- functionalExitCode: {report.get('exitCode')}",
        f"- counts: passed={counts.get('passed', 0)} failed={counts.get('failed', 0)} blocked={counts.get('blocked', 0)} timeout={counts.get('timeout', 0)} refused={counts.get('refused', 0)}",
        f"- binary sha256: {(report.get('binary') or {}).get('sha256')}",
        f"- datasetHash: {(report.get('fixture') or {}).get('datasetHash')}",
        f"- historicalBinary: {report.get('historicalBinary')}",
        "",
        "## Steps",
        "",
    ]
    for step in report.get("steps") or []:
        failure = "; ".join(step.get("failures") or [])[:400]
        lines.append(f"- `{step.get('id')}` {step.get('status')}" + (f" — {failure}" if failure else ""))
    lines.extend(
        [
            "",
            "## Graph",
            "",
            str((report.get("graph") or {}).get("gap")),
            "",
            "## Functional Problems",
            "",
        ]
    )
    for item in report.get("functionalProblems") or []:
        lines.append(f"- {item}")
    lines.extend(
        [
            "",
            "## Release Requirements (Pending Multi-Platform / Soak / Graph Probes)",
            "",
        ]
    )
    for item in report.get("releaseRequirements") or report.get("acceptanceProblems") or []:
        lines.append(f"- {item}")
    lines.append("")
    return "\n".join(lines)


def driver_head() -> str | None:
    try:
        return subprocess.run(
            ["git", "-C", SCRIPTS_DIR, "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        ).stdout.strip() or None
    except (OSError, subprocess.TimeoutExpired):
        return None


def main(argv: Sequence[str] | None = None) -> int:
    install_resource_warning_filter()
    display_before = parent_display()
    parser = build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    output: Path | None = None
    try:
        timeout = parse_timeout(args.timeout)
        binary = require_absolute(args.binary, "binary")
        fixture = require_absolute(args.fixture, "fixture")
        output = require_absolute(args.output, "output")
        require_tools()
        binary_info = check_binary(binary, args.binary_sha)
        reject_output(output, fixture)
        manifest = accept_baseline(fixture, args.dataset_hash, verify)
        native = load_native()
        receipt = require_absolute(args.helper_receipt, "helper receipt") if args.helper_receipt else None
        helpers = helper_hashes(native, receipt)
        output.mkdir(parents=True)
        config = {
            "native": native,
            "manifest": manifest,
            "output": output,
            "timeout": timeout,
            "binary": binary_info,
            "fixture": str(fixture),
            "dataset_hash": args.dataset_hash.lower(),
            "helpers": helpers,
            "phase": "all" if args.steps else args.phase,
            "step_ids": selected_ids(manifest, args.phase, args.steps),
            "driverHead": driver_head(),
        }
        report = run_steps(config)
        assert_parent_display_unchanged(display_before, parent_display())
        print(f"report={output / 'report.json'}", flush=True)
        print(f"exit={report['exitCode']} counts={report['counts']}", flush=True)
        return int(report["exitCode"])
    except DriverError as exc:
        print(f"error: {exc}", file=sys.stderr)
        if output is not None and output.is_dir():
            failure = {
                "acceptanceComplete": False,
                "historicalBinary": True,
                "synthetic": False,
                "memoryAbsenceClaimed": False,
                "releaseClaimed": False,
                "failures": [str(exc)],
                "steps": [],
                "graph": graph_from_lines([]),
                "platform": {"complete": False, "executed": ["linux"]},
                "memoryGate": {"executed": False},
            }
            failure["exitCode"] = acceptance_exit_code(failure)
            write_json(output / "report.json", failure)
        return 1
    finally:
        assert_parent_display_unchanged(display_before, parent_display())


if __name__ == "__main__":
    sys.exit(main())
