#!/usr/bin/env python3
"""Fail-closed tests for the native collaboration UI driver. No display and no binary."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import sys
import tempfile
import unittest
import warnings
from contextlib import ExitStack
from types import SimpleNamespace
from unittest.mock import patch
from pathlib import Path

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

import check_native_collaboration as driver


class _Proc:
    def poll(self):
        return None


class _Session:
    def __init__(self, lines: list[str] | list[tuple[float, str]]):
        if lines and isinstance(lines[0], str):
            self.lines = [(float(i), line) for i, line in enumerate(lines)]
        else:
            self.lines = list(lines)
        self.proc = _Proc()

    def texts(self, start: int = 0) -> list[str]:
        return [t for _, t in self.lines[start:]]

    def wait_line(self, predicate, start: int = 0, timeout: float = 1.0):
        for idx, (_, line) in enumerate(self.lines[start:], start=start):
            if predicate(line):
                return idx, 0.0, line
        raise TimeoutError("timed out")

    def capture(self, win, name, timeout: float = 1.0) -> dict[str, str]:
        return {"png": f"{name}.png"}


def _passed_step(step_id: str) -> dict:
    is_neg = step_id.startswith("neg-")
    valid_sha = "a" * 64
    if is_neg:
        cb = {"sourceSha256": valid_sha, "refused": "test-refusal"}
        cmp_applied = "equals-baseline"
    else:
        cb = {
            "source": {"sha256": valid_sha, "bytes": 10},
            "readback": {"sha256": valid_sha, "bytes": 10},
        }
        cmp_applied = "ok"
    outcomes = {
        "neg-stale-target": ("apply", "[APP:PASTE_STALE_DETECTED: changed]"),
        "neg-cancel": ("cancel", "[APP:PASTE_CANCELLED]"),
        "neg-overwrite-unauthorized": ("apply", "[APP:PASTE_DONE: created=0 overwritten=0 skipped=1]"),
    }
    if step_id in outcomes:
        field, value = outcomes[step_id]
        cb[field] = value
    if step_id == "neg-mapping-missing-destination":
        cb.update({
            "prevention": "canonical-destination-whitelist", "originalMappedIdSubmitted": False,
            "automaticMappingObserved": False, "disabledApplyClicked": True,
            "refusal": "[APP:PASTE_ERR: mapping_required]", "clipboardMatchesSource": True,
            "readbackSha256": valid_sha, "missingDestRepoId": "b-does-not-exist",
            "candidateCanonicalRoots": [f"/fixture/machine-b/repo-{i}" for i in range(15)],
            "workspaceCanonicalRoots": [f"/fixture/machine-b/repo-{i}" for i in range(15)],
            "candidateRepoIds": [f"b-repo-{i}" for i in range(15)],
        })
    return {
        "id": step_id,
        "kind": "negative" if is_neg else "positive-commit" if step_id.startswith("commit-") else "positive-file",
        "status": "passed",
        "actions": [{"action": "copy"}],
        "probeGenerations": ["[APP:COPY_DONE: copied=1]"],
        "source": {"repoId": "a-west-billing"},
        "clipboard": cb,
        "screenshots": {"result": {"png": "result.png"}},
        "hashes": {"binary": valid_sha},
        "snapshots": {"before": "before.json", "after": "after.json"},
        "compare": {"applied": cmp_applied},
        "failures": [],
        "cleanup": {
            "errors": [],
            "survivors": [],
            "appExit": 0,
            "forced": False,
            "drained": True,
            "apps": [
                {"app": {"pid": 100, "starttime": 1234}, "appExit": 0, "forced": False, "drained": True, "errors": [], "survivors": []},
                {"app": {"pid": 101, "starttime": 1235}, "appExit": 0, "forced": False, "drained": True, "errors": [], "survivors": []},
            ],
        },
    }


def _valid_report(steps: list[dict] | None = None) -> dict:
    valid_sha = "a" * 64
    if steps is None:
        steps = [_passed_step(sid) for sid in driver.REQUIRED_STEP_IDS]
    return {
        "scope": "linux-functional",
        "binary": "/path/to/binary",
        "binarySha256": valid_sha,
        "datasetHash": valid_sha,
        "steps": steps,
        "graph": {"paintedEdgesProven": False},
        "platform": {"complete": False, "executed": ["linux"], "required": ["linux", "windows", "macos"]},
        "historicalBinary": True,
        "memoryGate": {"executed": False},
        "memoryAbsenceClaimed": False,
        "acceptanceComplete": False,
    }


class ValidationTests(unittest.TestCase):
    def test_bad_sha_and_missing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"not-the-app")
            digest = hashlib.sha256(b"not-the-app").hexdigest()
            self.assertEqual(driver.check_binary(path, digest)["sha256"], digest)
            with self.assertRaises(driver.DriverError):
                driver.check_binary(path, "0" * 64)
            with self.assertRaises(driver.DriverError):
                driver.check_binary(path, "zz")
            with self.assertRaises(driver.DriverError):
                driver.check_binary(Path(tmp) / "missing", digest)

    def test_dataset_hash_mismatch_does_not_call_verifier(self) -> None:
        called: list[str] = []

        def verifier(path: str) -> dict:
            called.append(path)
            return {"datasetHash": "ff" * 32}

        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp)
            (fixture / "manifest.json").write_text(
                json.dumps({"datasetHash": "ab" * 32, "summary": {}, "steps": []}),
                encoding="utf-8",
            )
            with self.assertRaises(driver.DriverError):
                driver.accept_baseline(fixture, "cd" * 32, verifier)
        self.assertEqual(called, [])

    def test_failed_verify_is_not_success(self) -> None:
        digest = "ab" * 32

        def verifier(_path: str) -> dict:
            raise driver.VerificationError("dirty baseline")

        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp)
            (fixture / "manifest.json").write_text(json.dumps({"datasetHash": digest}), encoding="utf-8")
            with self.assertRaises(driver.DriverError) as caught:
                driver.accept_baseline(fixture, digest, verifier)
        self.assertIn("verify failed", str(caught.exception))

    def test_timeout_rejects_non_finite(self) -> None:
        self.assertEqual(driver.parse_timeout("15"), 15.0)
        for value in (0, -1, True, False, None, "inf", "nan", "nope"):
            with self.assertRaises(driver.DriverError):
                driver.parse_timeout(value)

    def test_missing_tools(self) -> None:
        with self.assertRaises(driver.DriverError) as caught:
            driver.require_tools(["Xvfb", "not-a-real-tool"], lookup=lambda name: "/usr/bin/Xvfb" if name == "Xvfb" else None)
        self.assertIn("not-a-real-tool", str(caught.exception))
        self.assertIn("refusing to install", str(caught.exception))

    def test_output_rejects_symlink_and_overlap(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture = root / "fixture"
            fixture.mkdir()
            output = root / "out"
            output.symlink_to(fixture)
            with self.assertRaises(driver.DriverError):
                driver.reject_output(output, fixture)
            nested = fixture / "inside"
            nested.mkdir()
            with self.assertRaises(driver.DriverError):
                driver.reject_output(nested, fixture)

    def test_relative_paths_rejected(self) -> None:
        with self.assertRaises(driver.DriverError):
            driver.require_absolute("relative/binary", "binary")


class SelectionTests(unittest.TestCase):
    def test_duplicate_display_names_use_workspace_relative_paths(self) -> None:
        names = driver.ui_repo_names(
            [
                {"repoId": "a-west-billing", "basename": "billing", "relativePath": "machine-a/west/billing"},
                {"repoId": "a-east-billing", "basename": "billing", "relativePath": "machine-a/east/billing"},
                {"repoId": "b-north-ledger", "basename": "ledger", "relativePath": "machine-b/north/ledger"},
            ]
        )
        self.assertEqual(names["a-west-billing"], "west/billing")
        self.assertEqual(names["a-east-billing"], "east/billing")
        self.assertIn("west/billing", names.values())

    def test_pick_ids_keep_index_and_basename_separate(self) -> None:
        self.assertEqual(driver.control_basename("pick-repo:4:billing"), "billing")
        self.assertEqual(driver.control_basename("pick-repo:3:billing"), "billing")
        self.assertEqual(driver.control_basename("pick-repo:ledger"), "ledger")
        self.assertTrue(driver.indexed_pick("pick-repo:4:billing"))
        self.assertFalse(driver.indexed_pick("pick-repo:ledger"))
        name, root = driver.selecting_fields("[APP:REPO_SELECTING: 4 (billing) root=/work/machine-a/west/billing]")
        self.assertEqual(name, "billing")
        self.assertTrue(root.endswith("west/billing"))

    def test_discovery_keeps_duplicate_basenames(self) -> None:
        lines = ["[APP:READY_REPOS: 3]"]
        lines += [
            "[APP:E2E_REPO: name=billing ok=true branch=main staged=0 unstaged=1 untracked=0 conflicts=0]",
            "[APP:E2E_REPO: name=billing ok=true branch=main staged=0 unstaged=1 untracked=0 conflicts=0]",
            "[APP:E2E_REPO: name=api ok=true branch=main staged=0 unstaged=1 untracked=0 conflicts=0]",
        ]
        found = driver.discover(_Session(lines), ["billing", "billing", "api"], timeout=1)
        self.assertEqual(found.count("billing"), 2)

    def test_missing_control_timeout_and_clipboard_mismatch_are_not_passes(self) -> None:
        self.assertEqual(driver.classify_exception(driver.MissingControl("btn-copy")), "blocked")
        self.assertEqual(driver.classify_exception(driver.OperationTimeout("timed out")), "timeout")
        self.assertFalse(driver.counts_as_pass("blocked"))
        self.assertFalse(driver.counts_as_pass("timeout"))
        self.assertFalse(driver.counts_as_pass("skipped"))
        self.assertFalse(driver.counts_as_pass("unsupported"))
        with self.assertRaises(driver.MissingControl):
            driver.change_control("fixed-oid", "src/keep.txt")
        with self.assertRaises(driver.ClipboardMismatch):
            driver.bridge_clipboard(b"payload", b"payload-x")
        source = _Clip(b"same")
        dest = _Clip(b"")
        dest.mutate = b"-mutated"
        with self.assertRaises(driver.ClipboardMismatch):
            driver.transfer_os_clipboard(source, dest)

    def test_click_control_waits_only_for_current_nonviewport_bounds(self) -> None:
        import bench_native_memory as native

        oid = "38a46ceaf58b6eab895d12b4894d4b6cf38d8a0d"
        control = f"btn-browse-tree:{oid}"
        # CI emitted preview completion 44.7 ms before this ready prepaint probe.
        preview = f"[APP:E2E_PREVIEW: source=commit_diff rev={oid} path=src/keep.txt]"
        bounds = f"[APP:CTRL_BOUNDS: id={control} x=907 y=203 w=72 h=22]"
        cases = (
            ("delayed", [bounds], None, 0.05),
            ("never", [], driver.MissingControl, 0.2),
            ("wrong-revision", [bounds.replace(oid, "f" * 40)], driver.MissingControl, 0.2),
            ("gone", [bounds, f"[APP:CTRL_GONE: id={control}]"], driver.MissingControl, 0.2),
            ("empty", [bounds.replace("w=72", "w=0")], driver.MissingControl, 0.05),
            ("off-window", [bounds.replace("x=907", "x=1070")], native.NativeBenchError, 0.05),
        )
        for name, frame, error, expected_elapsed in cases:
            with self.subTest(case=name):
                elapsed = [0.0]
                clicks = []
                session = _Session([preview])
                session.click = lambda win, box: clicks.append(box)

                def sleep(delay):
                    elapsed[0] += delay
                    if len(session.lines) == 1 and elapsed[0] >= 0.0447:
                        session.lines.extend((elapsed[0], line) for line in frame)

                win = {"width": 1080, "height": 720}
                with patch.object(driver.time, "monotonic", side_effect=lambda: elapsed[0]), patch.object(driver.time, "sleep", side_effect=sleep):
                    if error is None:
                        driver.click_control(native, session, win, control, timeout=0.2)
                        self.assertEqual(clicks, [(907, 203, 72, 22)])
                    else:
                        with self.assertRaises(error):
                            driver.click_control(native, session, win, control, timeout=0.2)
                        self.assertEqual(clicks, [])
                self.assertAlmostEqual(elapsed[0], expected_elapsed)

    def test_graph_counts_do_not_prove_edges(self) -> None:
        observed = driver.graph_from_lines(["[APP:GRAPH_LOADED: commits=14]", "[APP:E2E_LOG: mode=graph n=14 first=c4ad19b page=1]"])
        self.assertFalse(observed["paintedEdgesProven"])
        self.assertEqual(len(observed["loaded"]), 2)
        self.assertEqual(observed["edgeProbes"], [])

    def test_working_deletion_provenance_resolves_to_unstaged(self) -> None:
        op = {
            "op": "delete",
            "source": {
                "kind": "working",
                "path": "src/extra.txt",
                "status": "D",
                "diffAgainst": "index",
                "mode": "100644",
                "oid": "1" * 40,
                "inIndex": True,
                "inWorktree": False,
            },
            "dest": {"repoId": "b-north-ledger", "path": "src/extra.txt"},
        }
        source_kind, path = driver.resolve_operation_source(op)
        self.assertEqual(source_kind, "unstaged")
        self.assertEqual(path, "src/extra.txt")
        row_id, chk_id = driver.change_control(source_kind, path)
        self.assertEqual(row_id, "change-row:unstaged:src/extra.txt")
        self.assertEqual(chk_id, "change-chk:unstaged:src/extra.txt")

    def test_index_deletion_provenance_resolves_to_staged(self) -> None:
        op = {
            "op": "delete",
            "source": {
                "kind": "index",
                "path": "notes/guide.txt",
                "status": "D",
                "diffAgainst": "HEAD",
                "mode": "100644",
                "oid": "2" * 40,
                "headOid": "2" * 40,
                "inIndex": False,
                "inWorktree": False,
            },
            "dest": {"repoId": "a-west-billing", "path": "notes/guide.txt"},
        }
        source_kind, path = driver.resolve_operation_source(op)
        self.assertEqual(source_kind, "staged")
        self.assertEqual(path, "notes/guide.txt")
        row_id, chk_id = driver.change_control(source_kind, path)
        self.assertEqual(row_id, "change-row:staged:notes/guide.txt")
        self.assertEqual(chk_id, "change-chk:staged:notes/guide.txt")

    def test_deletion_operation_source_kind_delete_is_rejected(self) -> None:
        op = {
            "op": "delete",
            "source": {"kind": "delete", "path": "src/extra.txt"},
        }
        with self.assertRaises(driver.DriverError):
            driver.resolve_operation_source(op)
        with self.assertRaises(driver.DriverError):
            driver.change_control("delete", "src/extra.txt")

    def test_deletion_provenance_conflicting_or_invalid_fails(self) -> None:
        bad_worktree = {
            "op": "delete",
            "source": {
                "kind": "working",
                "path": "src/extra.txt",
                "status": "D",
                "diffAgainst": "index",
                "oid": "1" * 40,
                "inIndex": True,
                "inWorktree": True,
            },
        }
        with self.assertRaises(driver.DriverError):
            driver.resolve_operation_source(bad_worktree)

        bad_diff = {
            "op": "delete",
            "source": {
                "kind": "working",
                "path": "src/extra.txt",
                "status": "D",
                "diffAgainst": "HEAD",
                "oid": "1" * 40,
                "inIndex": True,
                "inWorktree": False,
            },
        }
        with self.assertRaises(driver.DriverError):
            driver.resolve_operation_source(bad_diff)

        no_oid = {
            "op": "delete",
            "source": {
                "kind": "working",
                "path": "src/extra.txt",
                "status": "D",
                "diffAgainst": "index",
                "inIndex": True,
                "inWorktree": False,
            },
        }
        with self.assertRaises(driver.DriverError):
            driver.resolve_operation_source(no_oid)

    def test_fixed_oid_controls_and_error_on_change_control(self) -> None:
        row, chk = driver.fixed_oid_controls("c0ffee" * 6 + "1234", "src/util.rs")
        self.assertEqual(row, "rev-row:src/util.rs")
        self.assertEqual(chk, f"rev-chk:{'c0ffee' * 6 + '1234'}:src/util.rs")
        with self.assertRaises(driver.MissingControl) as caught:
            driver.change_control("fixed-oid", "src/util.rs")
        self.assertIn("rev-chk", str(caught.exception))

    def test_parse_paste_map_candidates(self) -> None:
        lines = [
            "[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path=/work/machine-b/east/billing]",
            "[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=1 path=/work/machine-b/west/billing]",
            "[APP:PASTE_MAP_CANDIDATE: prefix=ledger idx=0 path=/work/machine-b/north/ledger]",
        ]
        candidates = driver.parse_paste_map_candidates(lines)
        self.assertEqual(len(candidates["billing"]), 2)
        self.assertEqual(candidates["billing"][0], (0, "/work/machine-b/east/billing"))
        self.assertEqual(candidates["billing"][1], (1, "/work/machine-b/west/billing"))
        self.assertEqual(candidates["ledger"], [(0, "/work/machine-b/north/ledger")])

    def test_resolve_paste_mappings_disambiguates_by_canonical_dest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            dest_dir = tmp_path / "machine-b" / "west" / "billing"
            dest_dir.mkdir(parents=True)
            other_dir = tmp_path / "machine-b" / "east" / "billing"
            other_dir.mkdir(parents=True)
            manifest = {
                "repos": [
                    {
                        "repoId": "b-west-billing",
                        "basename": "billing",
                        "relativePath": "machine-b/west/billing",
                    },
                    {
                        "repoId": "b-east-billing",
                        "basename": "billing",
                        "relativePath": "machine-b/east/billing",
                    },
                ]
            }
            lines = [
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path={other_dir}]",
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=1 path={dest_dir}]",
            ]
            clicks: list[str] = []

            class MockNative:
                def parse_bounds(self, texts):
                    return {
                        "paste-mappings": (10, 10, 200, 200),
                        "paste-map-pick:billing:0": (20, 20, 50, 20),
                        "paste-map-pick:billing:1": (20, 50, 50, 20),
                    }

                def require_control(self, texts, control):
                    return (20, 50, 50, 20)

                def assert_on_window(self, box, win, control):
                    pass

            class MockSession:
                def __init__(self, texts_list):
                    self.lines = [(0.0, t) for t in texts_list]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def click(self, win, box):
                    clicks.append("clicked")
                    self.lines.append((0.0, f"[APP:PASTE_MAPPED: prefix=billing dest={dest_dir} items=1]"))

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError("timed out")

            sess = MockSession(lines)
            win = {"wid": 1, "x": 0, "y": 0, "w": 800, "h": 600}
            trace: list[dict] = []
            driver.resolve_paste_mappings(
                MockNative(),
                sess,
                win,
                manifest,
                "b-west-billing",
                tmp_path,
                timeout=1.0,
                trace=trace,
            )
            self.assertEqual(len(clicks), 1)
            self.assertEqual(trace[0]["control"], "paste-map-pick:billing:1")


class _Clip:
    def __init__(self, payload: bytes):
        self.payload = payload
        self.mutate = b""

    def read_clipboard(self) -> bytes:
        return self.payload

    def set_clipboard(self, payload: bytes) -> None:
        self.payload = bytes(payload) + self.mutate


class ReportTests(unittest.TestCase):
    def test_cleanup_failure_demotes_a_pass(self) -> None:
        step = _passed_step("commit-a-to-b-pair02")
        driver.apply_cleanup(step, ["xclip pid 9 still alive"], [])
        self.assertEqual(step["status"], "failed")
        self.assertTrue(step["cleanup"]["errors"])

    def test_report_omissions_and_incomplete_subset_are_nonzero(self) -> None:
        step = _passed_step("file-b-to-a-pair09")
        del step["clipboard"]
        self.assertIn("clipboard", driver.evidence_omissions(step))
        report = {
            "steps": [_passed_step(step_id) for step_id in driver.PILOT_IDS],
            "graph": {"paintedEdgesProven": False},
            "platform": {"complete": False, "executed": ["linux"]},
            "historicalBinary": True,
            "memoryGate": {"executed": False},
            "memoryAbsenceClaimed": False,
            "acceptanceComplete": False,
        }
        problems = driver.acceptance_problems(report)
        self.assertTrue(problems)
        self.assertEqual(driver.acceptance_exit_code(report), 1)
        self.assertTrue(any("18" in item for item in problems))

    def test_synthetic_completeness_cannot_pass(self) -> None:
        steps = [_passed_step(f"step-{index}") for index in range(18)]
        report = {
            "steps": steps,
            "graph": {"paintedEdgesProven": True},
            "platform": {"complete": True},
            "historicalBinary": False,
            "memoryGate": {"executed": True},
            "memoryAbsenceClaimed": False,
            "syntheticCompleteness": True,
            "acceptanceComplete": True,
        }
        self.assertEqual(driver.acceptance_exit_code(report), 1)
        self.assertTrue(any("synthetic" in item for item in driver.acceptance_problems(report)))

    def test_memory_absence_claim_is_rejected(self) -> None:
        report = {
            "steps": [_passed_step(f"step-{index}") for index in range(18)],
            "graph": {"paintedEdgesProven": True},
            "platform": {"complete": True},
            "historicalBinary": False,
            "memoryGate": {"executed": True},
            "memoryAbsenceClaimed": True,
        }
        self.assertEqual(driver.acceptance_exit_code(report), 1)

    def test_cli_defaults_do_not_bake_temporary_paths(self) -> None:
        parser = driver.build_parser()
        for action in parser._actions:
            if isinstance(action.default, str):
                self.assertNotIn("/tmp/", action.default)
        source = Path(driver.__file__).read_text(encoding="utf-8")
        self.assertNotRegex(source, r"\[\s*[\"']xclip")
        self.assertNotIn("/tmp/snip-native", source)
        self.assertIn("acceptanceComplete", source)

    def test_blank_step_is_not_passed(self) -> None:
        self.assertEqual(driver.blank_step("file-a-to-b-pair01")["status"], "not-run")
        self.assertFalse(driver.counts_as_pass("not-run"))

    def test_resource_warning_is_an_error(self) -> None:
        with warnings.catch_warnings():
            driver.install_resource_warning_filter()
            with self.assertRaises(ResourceWarning):
                warnings.warn("leaked handle", ResourceWarning)

    def test_linux_functional_scope_passes_with_all_18_cases_while_release_remains_blocked(self) -> None:
        report = _valid_report()
        func_problems = driver.functional_problems(report)
        self.assertEqual(func_problems, [])
        self.assertEqual(driver.functional_exit_code(report), 0)
        self.assertEqual(driver.acceptance_exit_code(report, scope="functional"), 0)

        rel_problems = driver.release_problems(report)
        self.assertTrue(rel_problems)
        self.assertTrue(any("painted" in p for p in rel_problems))
        self.assertTrue(any("platform" in p for p in rel_problems))
        self.assertTrue(any("memory" in p for p in rel_problems))
        self.assertEqual(driver.release_exit_code(report), 1)
        self.assertEqual(driver.acceptance_exit_code(report, scope="release"), 1)

    def test_pilot_subset_or_less_than_18_steps_fails_functional_gate(self) -> None:
        report = _valid_report(steps=[_passed_step(step_id) for step_id in driver.PILOT_IDS])
        self.assertEqual(driver.functional_exit_code(report), 1)
        self.assertTrue(any("18" in p for p in driver.functional_problems(report)))

    def test_blocked_step_fails_functional_gate(self) -> None:
        steps = [_passed_step(sid) for sid in driver.REQUIRED_STEP_IDS if sid != "neg-stale-source"]
        blocked = driver.blank_step("neg-stale-source")
        blocked["status"] = "blocked"
        blocked["failures"] = ["stale-source-freshness"]
        steps.append(blocked)
        report = _valid_report(steps=steps)
        self.assertEqual(driver.functional_exit_code(report), 1)
        self.assertTrue(any("neg-stale-source: blocked" in p for p in driver.functional_problems(report)))

    def test_acceptance_complete_true_while_release_unmet_fails_functional_gate(self) -> None:
        report = _valid_report()
        report["acceptanceComplete"] = True
        self.assertEqual(driver.functional_exit_code(report), 1)
        self.assertTrue(any("acceptanceComplete" in p for p in driver.functional_problems(report)))

    def test_empty_screenshot_is_evidence_omission(self) -> None:
        step = _passed_step("neg-mapping-collision")
        step["screenshots"] = {}
        omissions = driver.evidence_omissions(step)
        self.assertIn("empty:screenshots", omissions)

        report = _valid_report()
        report["steps"][0]["screenshots"] = {}
        self.assertEqual(driver.functional_exit_code(report), 1)
        self.assertTrue(any("evidence missing" in p for p in driver.functional_problems(report)))


class OrchestratorHistoricalTreeTests(unittest.TestCase):
    def test_select_fixed_oid_browse_tree_expansion_and_rev_chk(self) -> None:
        """Verify full historical sequence: select commit -> browse-tree -> REV_TREE/E2E_TREE -> expand parents -> rev-row -> PREVIEW_LOADED/E2E_PREVIEW -> rev-chk."""
        with tempfile.TemporaryDirectory() as tmp:
            repo_path = Path(tmp) / "repo"
            repo_path.mkdir()
            full_commit = "abcdef0123456789abcdef0123456789abcdef01"
            short_commit = full_commit[:7]
            full_blob = "1234567890abcdef1234567890abcdef12345678"
            file_path = "notes/guide/spec.txt"

            def mock_git_read(repo, args, timeout):
                cmd = " ".join(args)
                if "rev-list" in cmd:
                    return full_commit
                if f"{full_commit}:{file_path}" in cmd:
                    return full_blob
                if f"HEAD^{{commit}}" in cmd or full_commit in cmd:
                    return full_commit
                return full_commit

            class MockNative:
                def require_control(self, texts, control):
                    return (10, 10, 50, 20)

                def assert_on_window(self, box, win, control):
                    pass

                def parse_bounds(self, texts):
                    return {}

            class MockSession:
                def __init__(self):
                    self.lines: list[tuple[float, str]] = [
                        (0.0, f"[APP:REPO_SELECTING: 0 (repo) root={repo_path}]"),
                    ]
                    self.clicked: list[str] = []

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def focus(self, wid):
                    pass

                def click(self, win, box):
                    pass

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError(f"timed out waiting for line after {start}")

            sess = MockSession()
            orig_git_read = driver.git_read
            orig_click_ctrl = driver.click_control
            driver.git_read = mock_git_read

            try:
                def mock_click_control(native, session, win, control, timeout, viewport=None):
                    sess.clicked.append(control)
                    if control == f"commit-row:{short_commit}":
                        sess.lines.append((0.0, f"[APP:COMMIT_SELECTED: {short_commit}]"))
                        sess.lines.append((0.0, f"[APP:E2E_PREVIEW: source=commit_diff rev={full_commit} path=notes/guide/spec.txt lines=5 fnv=abc]"))
                    elif control == f"btn-browse-tree:{full_commit}":
                        self.assertIsNone(viewport, "browse-tree toolbar is outside the scrollable log list")
                        sess.lines.append((0.0, f"[APP:REV_TREE: {short_commit}]"))
                        sess.lines.append((0.0, f"[APP:E2E_TREE: rev={short_commit} dir=/ entries=2 fnv=123]"))
                    elif control == "rev-row:notes":
                        sess.lines.append((0.0, "[APP:TREE_EXPANDED: notes]"))
                    elif control == "rev-row:notes/guide":
                        sess.lines.append((0.0, "[APP:TREE_EXPANDED: notes/guide]"))
                    elif control == f"rev-row:{file_path}":
                        sess.lines.append((0.0, f"[APP:PREVIEW_LOADED: {file_path}]"))
                        sess.lines.append((0.0, f"[APP:E2E_PREVIEW: source=commit_file rev={full_commit} path={file_path} lines=5 fnv=abc]"))
                    elif control == f"rev-chk:{full_commit}:{file_path}":
                        sess.lines.append((0.0, "[APP:BASKET: n=1 ...]"))

                driver.click_control = mock_click_control

                win = {"wid": 1, "x": 0, "y": 0, "w": 800, "h": 600}
                trace: list[dict] = []
                driver.select_fixed_oid(
                    MockNative(),
                    sess,
                    win,
                    repo_path,
                    full_commit,
                    file_path,
                    full_blob,
                    timeout=1.0,
                    trace=trace,
                )

                self.assertIn(f"commit-row:{short_commit}", sess.clicked)
                self.assertEqual(sess.clicked.count(f"btn-browse-tree:{full_commit}"), 1)
                self.assertIn("rev-row:notes", sess.clicked)
                self.assertIn("rev-row:notes/guide", sess.clicked)
                self.assertIn(f"rev-row:{file_path}", sess.clicked)
                self.assertIn(f"rev-chk:{full_commit}:{file_path}", sess.clicked)
                actions = [t["action"] for t in trace]
                self.assertEqual(actions, ["commit-click", "browse-tree", "expand-rev", "expand-rev", "rev-nav", "check-fixed-oid"])

            finally:
                driver.git_read = orig_git_read
                driver.click_control = orig_click_ctrl

    def test_select_fixed_oid_fails_on_wrong_active_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo_path = Path(tmp) / "repo"
            repo_path.mkdir()
            full_commit = "a" * 40

            def mock_git_read(repo, args, timeout):
                cmd = " ".join(args)
                if "rev-list" in cmd:
                    return f"{full_commit}\n"
                return full_commit

            class MockSession:
                def __init__(self):
                    self.lines: list[tuple[float, str]] = [
                        (0.0, "[APP:REPO_SELECTING: 0 (repo) root=/wrong/path]"),
                    ]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError(f"timed out waiting for line after {start}")

            sess = MockSession()
            orig_git_read = driver.git_read
            orig_click_ctrl = driver.click_control
            driver.git_read = mock_git_read

            def mock_click_control(native, session, win, control, timeout, viewport=None):
                if control.startswith("commit-row:"):
                    session.lines.append((0.0, f"[APP:COMMIT_SELECTED: {full_commit[:7]}]"))
                    session.lines.append((0.0, f"[APP:COMMIT_FOCUS: {full_commit}]"))

            driver.click_control = mock_click_control
            try:
                with self.assertRaises(driver.DriverError) as caught:
                    driver.select_fixed_oid(
                        None,
                        sess,
                        {"wid": 1},
                        repo_path,
                        full_commit,
                        "a.txt",
                        "b" * 40,
                        timeout=1.0,
                        trace=[],
                    )
                self.assertIn("wrong root", str(caught.exception))
            finally:
                driver.git_read = orig_git_read
                driver.click_control = orig_click_ctrl

    def test_select_fixed_oid_fails_on_blob_oid_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo_path = Path(tmp) / "repo"
            repo_path.mkdir()
            full_commit = "a" * 40

            def mock_git_read(repo, args, timeout):
                cmd = " ".join(args)
                if "rev-list" in cmd:
                    return f"{full_commit}\n"
                if ":" in cmd:
                    return "actual_blob_oid"
                return full_commit

            class MockSession:
                def __init__(self):
                    self.lines: list[tuple[float, str]] = [
                        (0.0, f"[APP:REPO_SELECTING: 0 (repo) root={repo_path}]"),
                    ]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError(f"timed out waiting for line after {start}")

            sess = MockSession()
            orig_git_read = driver.git_read
            orig_click_ctrl = driver.click_control
            driver.git_read = mock_git_read

            def mock_click_control(native, session, win, control, timeout, viewport=None):
                if control.startswith("commit-row:"):
                    session.lines.append((0.0, f"[APP:COMMIT_SELECTED: {full_commit[:7]}]"))
                    session.lines.append((0.0, f"[APP:COMMIT_FOCUS: {full_commit}]"))

            driver.click_control = mock_click_control
            try:
                with self.assertRaises(driver.DriverError) as caught:
                    driver.select_fixed_oid(
                        None,
                        sess,
                        {"wid": 1},
                        repo_path,
                        full_commit,
                        "a.txt",
                        "expected_blob_oid",
                        timeout=1.0,
                        trace=[],
                    )
                self.assertIn("manifest blob OID", str(caught.exception))
            finally:
                driver.git_read = orig_git_read
                driver.click_control = orig_click_ctrl

    def test_leave_rev_tree_if_open(self) -> None:
        clicked = []
        def click(native, session, win, control, timeout, **kwargs):
            clicked.append(control)
            session.add("[APP:REV_TREE: off]" if control == "btn-leave-tree" else "[APP:TAB_SWITCHED: GitChanges visible=true selected=1]")
        with patch.object(driver, "click_control", side_effect=click):
            s1 = _FlowSession(["[APP:REV_TREE: abc1234]"])
            driver.leave_rev_tree_if_open(None, s1, {"wid": 1}, 1.0, [])
            self.assertEqual(clicked, ["btn-leave-tree", "rail-changes"])

            clicked.clear()
            s2 = _FlowSession(["[APP:REV_TREE: abc1234]", "[APP:REV_TREE: off]"])
            driver.leave_rev_tree_if_open(None, s2, {"wid": 1}, 1.0, [])
            self.assertEqual(clicked, [])

    def test_leaving_rev_tree_requires_fresh_visible_changes_event(self) -> None:
        for event in (None, "[APP:TAB_SWITCHED: FileExplorer visible=true selected=1]", "[APP:TAB_SWITCHED: GitChanges visible=false selected=1]"):
            with self.subTest(event=event):
                session = _FlowSession(["[APP:TAB_SWITCHED: GitChanges visible=true selected=1]", "[APP:REV_TREE: abc1234]"])
                def click(native, session, win, control, timeout, **kwargs):
                    if control == "btn-leave-tree":
                        session.add("[APP:REV_TREE: off]")
                    elif event:
                        session.add(event)
                with patch.object(driver, "click_control", side_effect=click), self.assertRaises(driver.DriverError):
                    driver.leave_rev_tree_if_open(None, session, {"wid": 1}, 1.0, [])


class OrchestratorPasteMappingTests(unittest.TestCase):
    def test_paste_mapping_current_generation_scoping(self) -> None:
        """Verifies candidate logs from previous generations are ignored when scoping to start_line."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            dest_dir = str(tmp_path / "target_root")
            stale_dir = str(tmp_path / "stale_root")
            os.makedirs(dest_dir, exist_ok=True)
            os.makedirs(stale_dir, exist_ok=True)

            manifest = {
                "repos": [{"repoId": "target-repo", "basename": "billing", "relativePath": "target_root"}]
            }
            # PASTE_MAP_CANDIDATE is emitted BEFORE PASTE_PREVIEW
            lines = [
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path={stale_dir}]",
                "[APP:PASTE_PREVIEW: items=1 dest=/old mapping=needed]",
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path={dest_dir}]",
                "[APP:PASTE_PREVIEW: items=1 dest=/new mapping=needed]",
            ]
            clicked_controls = []

            class MockNative:
                def parse_bounds(self, texts):
                    return {"paste-mappings": (0, 0, 100, 100), "paste-map-pick:billing:0": (0, 0, 10, 10)}

                def require_control(self, texts, control):
                    return (0, 0, 10, 10)

                def assert_on_window(self, box, win, control):
                    pass

            class MockSession:
                def __init__(self):
                    self.lines = [(0.0, line) for line in lines]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def click(self, win, box):
                    pass

                def wait_line(self, predicate, start=0, timeout=1.0):
                    msg = f"[APP:PASTE_MAPPED: prefix=billing dest={dest_dir} items=1]"
                    self.lines.append((0.0, msg))
                    return len(self.lines) - 1, 0.0, msg

            orig_click = driver.click_control
            def mock_click(native, session, win, control, timeout, viewport=None):
                clicked_controls.append(control)
                session.lines.append((0.0, f"[APP:PASTE_MAPPED: prefix=billing dest={dest_dir} items=1]"))

            driver.click_control = mock_click
            try:
                trace = []
                driver.resolve_paste_mappings(
                    MockNative(),
                    MockSession(),
                    {"wid": 1},
                    manifest,
                    "target-repo",
                    tmp_path,
                    timeout=1.0,
                    trace=trace,
                    start_line=2,
                )
                self.assertIn("paste-map-pick:billing:0", clicked_controls)
                self.assertEqual(trace[0]["dest"], dest_dir)
            finally:
                driver.click_control = orig_click

    def test_paste_mapping_fails_without_canonical_root_match_no_fallback(self) -> None:
        """Verifies no fallback to lone button if canonical root does not match."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            other_dir = str(tmp_path / "other")
            os.makedirs(other_dir, exist_ok=True)
            manifest = {
                "repos": [{"repoId": "my-dest", "basename": "billing", "relativePath": "missing_dest"}]
            }
            lines = [
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path={other_dir}]",
                "[APP:PASTE_PREVIEW: items=1 dest=/new mapping=needed]",
            ]
            class MockNative:
                def parse_bounds(self, texts):
                    return {"paste-mappings": (0, 0, 100, 100), "paste-map-pick:billing:0": (0, 0, 10, 10)}

            class MockSession:
                def __init__(self):
                    self.lines = [(0.0, line) for line in lines]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

            with self.assertRaises(driver.MissingControl) as caught:
                driver.resolve_paste_mappings(
                    MockNative(),
                    MockSession(),
                    {"wid": 1},
                    manifest,
                    "my-dest",
                    tmp_path,
                    timeout=1.0,
                    trace=[],
                )
            self.assertIn("cannot resolve destination root", str(caught.exception))

    def test_paste_mapping_fails_on_emitted_prefix_or_dest_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            dest_dir = str(tmp_path / "dest")
            os.makedirs(dest_dir, exist_ok=True)
            manifest = {
                "repos": [{"repoId": "my-dest", "basename": "billing", "relativePath": "dest"}]
            }
            lines = [
                f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx=0 path={dest_dir}]",
                "[APP:PASTE_PREVIEW: items=1 dest=/new mapping=needed]",
            ]
            class MockNative:
                def parse_bounds(self, texts):
                    return {"paste-mappings": (0, 0, 100, 100), "paste-map-pick:billing:0": (0, 0, 10, 10)}

            class MockSession:
                def __init__(self):
                    self.lines = [(0.0, line) for line in lines]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError(f"timed out waiting for line after {start}")

            orig_click = driver.click_control
            try:
                # Test prefix mismatch
                driver.click_control = lambda native, sess, win, ctrl, timeout, **kw: sess.lines.append(
                    (0.0, f"[APP:PASTE_MAPPED: prefix=wrong dest={dest_dir} items=1]")
                )
                with self.assertRaises(driver.DriverError) as caught:
                    driver.resolve_paste_mappings(MockNative(), MockSession(), {"wid": 1}, manifest, "my-dest", tmp_path, 1.0, [])
                self.assertIn("PASTE_MAPPED prefix mismatch", str(caught.exception))

                # Test dest mismatch
                driver.click_control = lambda native, sess, win, ctrl, timeout, **kw: sess.lines.append(
                    (0.0, f"[APP:PASTE_MAPPED: prefix=billing dest=/wrong/path items=1]")
                )
                with self.assertRaises(driver.DriverError) as caught:
                    driver.resolve_paste_mappings(MockNative(), MockSession(), {"wid": 1}, manifest, "my-dest", tmp_path, 1.0, [])
                self.assertIn("PASTE_MAPPED dest mismatch", str(caught.exception))
            finally:
                driver.click_control = orig_click

    def test_paste_mapping_preserves_spaces_and_unicode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            dest_dir = str(tmp_path / "專案 目錄")
            os.makedirs(dest_dir, exist_ok=True)
            manifest = {
                "repos": [{"repoId": "my-dest", "basename": "前綴 測試", "relativePath": "專案 目錄"}]
            }
            lines = [
                f"[APP:PASTE_MAP_CANDIDATE: prefix=前綴 測試 idx=0 path={dest_dir}]",
                "[APP:PASTE_PREVIEW: items=1 dest=/new mapping=needed]",
            ]
            class MockNative:
                def parse_bounds(self, texts):
                    return {"paste-mappings": (0, 0, 100, 100), "paste-map-pick:前綴 測試:0": (0, 0, 10, 10)}

            class MockSession:
                def __init__(self):
                    self.lines = [(0.0, line) for line in lines]

                def texts(self, start=0):
                    return [t for _, t in self.lines[start:]]

                def wait_line(self, predicate, start=0, timeout=1.0):
                    for idx, (_, line) in enumerate(self.lines[start:], start=start):
                        if predicate(line):
                            return idx, 0.0, line
                    raise TimeoutError(f"timed out waiting for line after {start}")

            orig_click = driver.click_control
            driver.click_control = lambda native, sess, win, ctrl, timeout, **kw: sess.lines.append(
                (0.0, f"[APP:PASTE_MAPPED: prefix=前綴 測試 dest={dest_dir} items=1]")
            )
            try:
                trace = []
                driver.resolve_paste_mappings(
                    MockNative(), MockSession(), {"wid": 1}, manifest, "my-dest", tmp_path, 1.0, trace
                )
                self.assertEqual(len(trace), 1)
                self.assertEqual(trace[0]["dest"], dest_dir)
            finally:
                driver.click_control = orig_click


class OrchestratorLifecycleTests(unittest.TestCase):
    def test_stop_one_graceful_ctrl_q_exit_success(self) -> None:
        keys_sent = []
        class MockProc:
            def poll(self):
                return None
            def wait(self, timeout=None):
                return 0

        class MockSession:
            def __init__(self):
                self.proc = MockProc()
                self.owned = [{"pid": 100, "starttime": 1234}]
                self.app = {"pid": 100, "starttime": 1234}
                self.lines = [
                    (0.0, "normal app line"),
                ]

            def remember_owned(self):
                pass

            def app_tree_pids(self):
                return [100]

            def window(self, timeout=None):
                return {"wid": "0x123"}

            def key(self, wid, key_combo):
                keys_sent.append((wid, key_combo))
                if key_combo == "ctrl+q":
                    self.lines.append((1.0, "phase=drained intent=quit jobs=0 inflight=0 queued=0 leaked=0 generation=3"))

            def texts(self, start: int = 0):
                return [text for _, text in self.lines[start:]]

            def stop(self):
                return []

        class MockNative:
            def read_proc_starttime(self, pid):
                return None  # Dead, no survivor

            def identity(self, pid):
                return {"pid": pid, "starttime": None}

        sess = MockSession()
        res = driver.stop_one(MockNative(), sess)
        self.assertEqual(keys_sent, [("0x123", "ctrl+q")])
        self.assertEqual(res["appExit"], 0)
        self.assertEqual(res["errors"], [])
        self.assertEqual(res["survivors"], [])

    def test_stop_one_nonzero_exit_recorded_as_error(self) -> None:
        class MockProc:
            def poll(self):
                return None
            def wait(self, timeout=None):
                return 137  # e.g. SIGKILL

        class MockSession:
            def __init__(self):
                self.proc = MockProc()
                self.owned = []
                self.app = None
                self.lines = [(0.0, "phase=drained intent=quit jobs=0")]

            def remember_owned(self):
                pass

            def window(self, timeout=None):
                return {"wid": "0x1"}

            def key(self, wid, k):
                pass

            def texts(self, start: int = 0):
                return [t for _, t in self.lines[start:]]

            def stop(self):
                return []

        res = driver.stop_one(None, MockSession())
        self.assertEqual(res["appExit"], 137)
        self.assertTrue(any("non-zero code 137" in e for e in res["errors"]))

    def test_stop_one_missing_drained_log_recorded_as_error(self) -> None:
        class MockProc:
            def poll(self):
                return None
            def wait(self, timeout=None):
                return 0

        class MockSession:
            def __init__(self):
                self.proc = MockProc()
                self.owned = []
                self.app = None
                self.lines = [(0.0, "premature exit without drained")]

            def remember_owned(self):
                pass

            def window(self, timeout=None):
                return {"wid": "0x1"}

            def key(self, wid, k):
                pass

            def texts(self, start: int = 0):
                return [t for _, t in self.lines[start:]]

            def stop(self):
                return []

        res = driver.stop_one(None, MockSession())
        self.assertEqual(res["appExit"], 0)
        self.assertTrue(any("missing fresh lifecycle drained zero-job evidence" in e for e in res["errors"]))

    def test_stop_one_detects_surviving_pid(self) -> None:
        class MockProc:
            def __init__(self):
                self.pid = 555
            def poll(self):
                return 0

        class MockSession:
            def __init__(self):
                self.proc = MockProc()
                self.owned = [{"pid": 555, "starttime": 9999}]
                self.app = {"pid": 555, "starttime": 9999}
                self.lines = [(0.0, "phase=drained intent=quit jobs=0")]

            def remember_owned(self):
                pass

            def app_tree_pids(self):
                return [555]

            def texts(self, start: int = 0):
                return [t for _, t in self.lines[start:]]

            def stop(self):
                return []

        class MockNative:
            def read_proc_starttime(self, pid):
                return 9999  # Still alive!
            def identity(self, pid):
                return {"pid": pid, "starttime": 9999}

        res = driver.stop_one(MockNative(), MockSession())
        self.assertEqual(len(res["survivors"]), 1)
        self.assertEqual(res["survivors"][0]["pid"], 555)
        self.assertTrue(any("surviving" in e and "555" in e for e in res["errors"]))

    def test_apply_cleanup_demotes_passed_step_on_error_or_nonzero_exit(self) -> None:
        step = _passed_step("file-a-to-b-pair01")
        driver.apply_cleanup(step, [], [], app_exit=1)
        self.assertEqual(step["status"], "failed")
        self.assertIn("cleanup failed", step["failures"])
        self.assertEqual(step["cleanup"]["appExit"], 1)


class OrchestratorValidationTests(unittest.TestCase):
    def test_valid_18_manifest_steps_pass_functional_gate(self) -> None:
        report = _valid_report()
        self.assertEqual(driver.functional_problems(report), [])
        self.assertEqual(driver.functional_exit_code(report), 0)
        self.assertEqual(driver.acceptance_exit_code(report, scope="functional"), 0)
        self.assertEqual(driver.release_exit_code(report), 1)

    def test_fabricated_step_ids_rejected(self) -> None:
        fake_steps = [_passed_step(f"step-{i:02d}") for i in range(18)]
        report = _valid_report(steps=fake_steps)
        problems = driver.functional_problems(report)
        self.assertTrue(any("step set does not match required 18 manifest step IDs" in p for p in problems))
        self.assertEqual(driver.functional_exit_code(report), 1)

    def test_non_empty_failures_fails_gate_even_if_status_passed(self) -> None:
        report = _valid_report()
        report["steps"][0]["failures"] = ["hidden error"]
        problems = driver.functional_problems(report)
        self.assertTrue(any("non-empty failures" in p for p in problems))
        self.assertEqual(driver.functional_exit_code(report), 1)

    def test_missing_binary_or_hashes_rejected(self) -> None:
        report = _valid_report()
        del report["binary"]
        self.assertIn("missing binary", driver.functional_problems(report))

        report2 = _valid_report()
        report2["binarySha256"] = "not_64_chars"
        self.assertTrue(any("binarySha256" in p for p in driver.functional_problems(report2)))

        report3 = _valid_report()
        report3["datasetHash"] = ""
        self.assertTrue(any("datasetHash" in p for p in driver.functional_problems(report3)))

    def test_invalid_clipboard_proof_rejected(self) -> None:
        # Positive step with bad sha256
        report = _valid_report()
        report["steps"][0]["clipboard"]["source"]["sha256"] = "short"
        self.assertTrue(any("positive step clipboard" in p for p in driver.functional_problems(report)))

        # Negative step with no proof
        report2 = _valid_report()
        neg_step = next(s for s in report2["steps"] if s["id"].startswith("neg-"))
        neg_step["clipboard"] = {}
        self.assertTrue(any("negative step clipboard lacks proof" in p for p in driver.functional_problems(report2)))

        # Negative step with bare prevention=True and no valid hash or refusal
        report3 = _valid_report()
        neg_step3 = next(s for s in report3["steps"] if s["id"].startswith("neg-"))
        neg_step3["clipboard"] = {"prevention": True}
        self.assertTrue(any("negative step clipboard lacks proof" in p for p in driver.functional_problems(report3)))

    def test_negative_step_without_baseline_compare_rejected(self) -> None:
        report = _valid_report()
        neg_step = next(s for s in report["steps"] if s["id"].startswith("neg-"))
        neg_step["compare"]["applied"] = "not-baseline"
        self.assertTrue(any("lacks no-write verification" in p for p in driver.functional_problems(report)))

    def test_nonzero_app_exit_or_surviving_pid_fails_gate(self) -> None:
        report = _valid_report()
        report["steps"][0]["cleanup"]["appExit"] = 1
        self.assertTrue(any("cleanup failed" in p for p in driver.functional_problems(report)))
        self.assertEqual(driver.functional_exit_code(report), 1)


class CleanupEvidenceTests(unittest.TestCase):
    def test_missing_or_invalid_cleanup_evidence_fails_gate(self):
        mutations = [
            lambda c: c.pop("apps"), lambda c: c.update(apps=[]),
            lambda c: c.update(apps=[c["apps"][0]]),
            lambda c: c.update(apps=[c["apps"][0], c["apps"][0]]),
            lambda c: c["apps"].__setitem__(0, None),
            lambda c: c["apps"][0].update(app=None),
            lambda c: c["apps"][0].update(errors=["nested failure"]),
            lambda c: c["apps"][0].pop("errors"),
            lambda c: c["apps"][0].pop("survivors"),
            lambda c: c["apps"][0].update(forced=True),
            lambda c: c["apps"][0].pop("drained"),
            lambda c: c["apps"][0].update(appExit=None),
            lambda c: c["apps"][0].update(appExit=False),
            lambda c: c.pop("drained"), lambda c: c.pop("errors"),
        ]
        for field in ("pid", "starttime"):
            for value in (None, 0, -1, True, 1.5, float("nan"), "123"):
                mutations.append(lambda c, field=field, value=value: c["apps"][0]["app"].update({field: value}))
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                report = _valid_report()
                mutate(report["steps"][0]["cleanup"])
                self.assertNotEqual(driver.functional_exit_code(report), 0)


class _FlowSession(_Session):
    def __init__(self, lines=()):
        super().__init__(list(lines))
        self.clipboard = b""
        self.key_result = "[APP:PASTE_ERR: mapping_required]"
        self.click_result = None

    def add(self, line):
        self.lines.append((float(len(self.lines)), line))

    def window(self, timeout=None):
        return {"wid": "0x1", "x": 0, "y": 0, "w": 100, "h": 100}

    def key(self, wid, key):
        if self.key_result:
            self.add(self.key_result)

    def click(self, win, box):
        if self.click_result:
            self.add(self.click_result)

    def set_clipboard(self, value):
        self.clipboard = value

    def read_clipboard(self):
        return self.clipboard


class ApplyRefusalTests(unittest.TestCase):
    def native(self):
        return SimpleNamespace(
            parse_bounds=lambda lines: {"btn-apply": (0, 0, 10, 10)},
            require_control=lambda lines, control: (0, 0, 10, 10),
            assert_on_window=lambda *args: None,
        )

    def session(self):
        return _FlowSession(["[APP:PASTE_PREVIEW: items=0 dest=/tmp mapping=false]"])

    def test_specific_keyboard_refusal_and_real_disabled_click(self):
        trace = []
        driver.try_apply_refusal(self.native(), self.session(), {"wid": "1"}, .001, trace)
        self.assertTrue(trace[-1]["clicked"])
        self.assertEqual(trace[-1]["line"], "[APP:PASTE_ERR: mapping_required]")

    def test_unexpected_apply_on_keyboard_or_disabled_click_fails(self):
        for key_result, click_result in (("[APP:PASTE_DONE: created=1]", None), ("[APP:PASTE_APPLYING]", None), ("[APP:PASTE_ERR: mapping_required]", "[APP:PASTE_DONE: created=1]")):
            with self.subTest(key=key_result, click=click_result):
                session = self.session()
                session.key_result, session.click_result = key_result, click_result
                with self.assertRaises(driver.DriverError):
                    driver.try_apply_refusal(self.native(), session, {"wid": "1"}, .001, [])

    def test_missing_or_off_window_apply_control_fails(self):
        for method in ("require_control", "assert_on_window"):
            native = self.native()
            setattr(native, method, lambda *args: (_ for _ in ()).throw(driver.DriverError("missing/off-window apply")))
            with self.subTest(method=method), self.assertRaises(driver.DriverError):
                driver.try_apply_refusal(native, self.session(), {"wid": "1"}, .001, [])

    def test_silence_generic_error_or_old_preview_cannot_pass(self):
        for result, cursor in ((None, 0), ("[APP:PASTE_ERR: clipboard]", 0), ("[APP:PASTE_ERR: mapping_required]", 1)):
            session = self.session()
            session.key_result = result
            with self.subTest(result=result, cursor=cursor), self.assertRaises(driver.DriverError):
                driver.try_apply_refusal(self.native(), session, {"wid": "1"}, .001, [], start_line=cursor)

    def test_delayed_unexpected_apply_during_observation_fails(self):
        session = self.session()
        ticks = iter([0.0, 0.0, .2, .4, 1.1])
        def late_event(_delay):
            session.add("[APP:PASTE_DONE: created=1]")
        with patch.object(driver.time, "monotonic", side_effect=lambda: next(ticks)), patch.object(driver.time, "sleep", side_effect=late_event):
            with self.assertRaises(driver.DriverError):
                driver.try_apply_refusal(self.native(), session, {"wid": "1"}, 2, [])


class CommitOracleTests(unittest.TestCase):
    def test_git_oracle_preserves_full_metadata_and_every_file(self):
        from collaboration_fixture import GitSession, git_available
        if not git_available():
            self.assertIsNone(os.environ.get("SNIP_REQUIRE_ALL_TESTS"), "Git is required")
            self.skipTest("Git is missing")
        with tempfile.TemporaryDirectory() as tmp, GitSession() as git:
            repo = Path(tmp)
            git.run(repo, ["init", "-q"])
            (repo / "old.txt").write_bytes(b"rename me\n")
            (repo / "gone.txt").write_bytes(b"delete me\n")
            git.run(repo, ["add", "."])
            git.run(repo, ["commit", "-q", "-m", "base"])
            base = git.rev_parse(repo, "HEAD")
            (repo / "old.txt").rename(repo / "new.txt")
            (repo / "gone.txt").unlink()
            (repo / "add.txt").write_bytes("full bytes\n尾行\n\n".encode())
            (repo / "binary.dat").write_bytes(b"a\0b")
            (repo / "invalid.dat").write_bytes(b"\xff")
            git.run(repo, ["add", "-A"])
            git.run(repo, ["commit", "-q", "--cleanup=verbatim", "-F", "-"], input_bytes=b"subject\n\nfull body\n\n", extra_env={"GIT_AUTHOR_NAME": "Author", "GIT_AUTHOR_EMAIL": "author@example.test", "GIT_AUTHOR_DATE": "2024-01-02T03:04:05+08:00"})
            tip = git.rev_parse(repo, "HEAD")
            expected = {"commits": [{
                "message": "subject\n\nfull body\n\n", "authorName": "Author", "authorEmail": "author@example.test", "authorDate": "2024-01-02T03:04:05+08:00",
                "files": [
                    {"path": "add.txt", "oldPath": None, "change": "ADDED", "content": "full bytes\n尾行\n\n", "notCopied": None},
                    {"path": "binary.dat", "oldPath": None, "change": "ADDED", "content": None, "notCopied": "BINARY"},
                    {"path": "gone.txt", "oldPath": None, "change": "DELETED", "content": None, "notCopied": None},
                    {"path": "invalid.dat", "oldPath": None, "change": "ADDED", "content": None, "notCopied": "NON_UTF8"},
                    {"path": "new.txt", "oldPath": "old.txt", "change": "RENAMED", "content": "rename me\n", "notCopied": None},
                ],
            }]}
            self.assertEqual(driver.commit_payload_oracle(repo, [tip], 5), expected)
            sequence = driver.commit_payload_oracle(repo, [base, tip], 5)
            self.assertEqual([c["message"] for c in sequence["commits"]], ["base\n", "subject\n\nfull body\n\n"])
            self.assertEqual([f["path"] for f in sequence["commits"][0]["files"]], ["gone.txt", "old.txt"])
            driver.require_exact_commit_payload(self.wire(expected), expected)
            mutations = [
                lambda p: p["commits"].append(copy.deepcopy(p["commits"][0])),
                lambda p: p["commits"].clear(),
                lambda p: p["commits"][0]["files"].pop(),
                lambda p: p["commits"][0]["files"].append(copy.deepcopy(p["commits"][0]["files"][0])),
            ]
            for field in ("message", "authorDate", "authorName", "authorEmail"):
                mutations.append(lambda p, field=field: p["commits"][0].update({field: "wrong"}))
            for field in ("path", "oldPath", "change", "content", "notCopied"):
                mutations.append(lambda p, field=field: p["commits"][0]["files"][0].update({field: "wrong"}))
            mutations.append(lambda p: p["commits"][0].update(message="subject\n\nfull body\n"))
            for mutation in mutations:
                altered = copy.deepcopy(expected)
                mutation(altered)
                with self.subTest(mutation=mutation), self.assertRaises(driver.DriverError):
                    driver.require_exact_commit_payload(self.wire(altered), expected)
            reversed_sequence = {"commits": list(reversed(sequence["commits"]))}
            with self.assertRaises(driver.DriverError):
                driver.require_exact_commit_payload(self.wire(reversed_sequence), sequence)

    @staticmethod
    def wire(payload):
        return b"// snip-sync commits v1\n" + json.dumps(payload, ensure_ascii=False).encode()

    def test_malformed_payload_is_not_an_oracle_match(self):
        for payload in (b"tip2 junk", b"// snip-sync commits v1\nno-json", b"// snip-sync commits v1\n[]"):
            with self.subTest(payload=payload), self.assertRaises(driver.DriverError):
                driver.require_exact_commit_payload(payload, {"commits": []})

    def test_cross_repo_flow_compares_export_and_saves_independent_oracle(self):
        expected = {"commits": [{"message": "second\n", "authorName": "A", "authorEmail": "a@b", "authorDate": "2024-01-01T00:00:00+00:00", "files": [{"path": "a.txt", "oldPath": None, "change": "ADDED", "content": "second repo", "notCopied": None}]}]}
        manifest = {"repos": [{"repoId": "first", "basename": "one", "relativePath": "machine-a/one"}, {"repoId": "second", "basename": "two", "relativePath": "machine-a/two"}]}
        step = {"tips": [{"repoId": "first", "rev": "tip"}, {"repoId": "second", "rev": "tip"}]}
        for extra in (False, True):
            actual = copy.deepcopy(expected)
            if extra:
                actual["commits"].append(copy.deepcopy(actual["commits"][0]))
            with tempfile.TemporaryDirectory() as tmp, ExitStack() as stack:
                trace = []
                stack.enter_context(patch.object(driver, "select_repo", side_effect=lambda *a: trace.append(a[3])))
                stack.enter_context(patch.object(driver, "click_commit_row", side_effect=lambda *a: trace.append(a[4])))
                stack.enter_context(patch.object(driver, "git_read", side_effect=lambda repo, args, timeout: "1" * 40 if repo.name == "one" else "2" * 40))
                oracle = stack.enter_context(patch.object(driver, "commit_payload_oracle", return_value=expected))
                stack.enter_context(patch.object(driver, "capture_checked", return_value={"png": "shot.png"}))
                stack.enter_context(patch.object(driver, "copy_commits", return_value=(self.wire(actual), "[APP:COPY_COMMITS_DONE: n=1]")))
                record = driver.blank_step("neg-cross-repo-commits")
                args = (None, {"a": _FlowSession()}, manifest, step, {}, Path(tmp), 1, [], record, Path(tmp))
                if extra:
                    with self.assertRaises(driver.DriverError):
                        driver.block_cross_repo(*args)
                    self.assertNotEqual(record["status"], "passed")
                else:
                    driver.block_cross_repo(*args)
                    self.assertTrue(record["clipboard"]["exactPayloadOracle"])
                    self.assertEqual(trace, ["one", "1" * 40, "two", "2" * 40])
                    self.assertEqual(json.loads((Path(tmp) / "commit-oracle.json").read_text())["payload"], expected)
                    oracle.assert_called_once_with(Path(tmp) / "machine-a/two", ["2" * 40], 1)


class MappingFlowTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.manifest = {"repos": [
            {"repoId": "a-west-billing", "basename": "billing", "relativePath": "machine-a/west/billing"},
            {"repoId": "a-east-billing", "basename": "billing", "relativePath": "machine-a/east/billing"},
            {"repoId": "a-west-docs", "basename": "docs", "relativePath": "machine-a/west/docs"},
            {"repoId": "b-north-ledger", "basename": "ledger", "relativePath": "machine-b/north/ledger"},
        ]}
        self.sessions = {"a": _FlowSession(), "b": _FlowSession()}
        self.native = SimpleNamespace(open_project_list=lambda *a: None)
        self.controls = []
        self.mapping_error = None
        self.error_prefix = "billing"
        self.wrong_dest = False
        self.emit_collision = True
        self.ambiguous = False
        self.only_one_candidate = False
        self.record = driver.blank_step("negative")
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        for name, replacement in {
            "select_repo": lambda *a: None,
            "click_control": self.click,
            "capture_checked": lambda *a: {"png": "shot.png"},
            "copy_from_button": lambda *a: b"payload",
            "transfer_os_clipboard": lambda *a: None,
            "paste_preview": self.preview,
            "apply_or_cancel": lambda *a: "[APP:PASTE_CANCELLED]",
        }.items():
            self.stack.enter_context(patch.object(driver, name, side_effect=replacement))

    def click(self, native, session, win, control, timeout, viewport=None):
        self.controls.append(control)
        if control.startswith("tree-row:"):
            session.add(f"[APP:TREE_EXPANDED: {control.split(':', 1)[1]}]")
        elif control.startswith("tree-chk:"):
            self.assertEqual(self.controls[-2], "tree-row:src")
            session.add("[APP:BASKET: n=1]")
        elif control.startswith(("paste-map-pick:", "paste-map-keep:")):
            prefix = control.split(":")[1]
            if self.mapping_error and prefix == self.error_prefix:
                session.add(self.mapping_error)
                return
            if prefix == "src" and self.emit_collision:
                session.add("[APP:PASTE_PLAN_REFUSED: reason=target_collision]")
            dest = "/wrong" if self.wrong_dest else str(self.root / "machine-b/north/ledger")
            keep = " keep=primary" if control.startswith("paste-map-keep:") else ""
            session.add(f"[APP:PASTE_MAPPED: prefix={prefix}{keep} dest={dest} items=0]")

    def preview(self, native, session, win, timeout, trace):
        if self.ambiguous:
            candidates = [self.root / "machine-a/west/billing"]
            if not self.only_one_candidate:
                candidates.append(self.root / "machine-a/east/billing")
        else:
            candidates = [self.root / "machine-b/north/ledger"]
        for prefix in ("billing", "src"):
            for idx, path in enumerate(candidates):
                session.add(f"[APP:PASTE_MAP_CANDIDATE: prefix={prefix} idx={idx} path={path}]")
        session.add("[APP:PASTE_PREVIEW: items=0 dest=/tmp mapping=false]")
        return session.texts()[-1]

    def collide(self):
        step = {"maps": [{"sourceRepoId": rid, "sourcePath": "src/app.txt", "destRepoId": "b-north-ledger", "destPath": "src/app.txt"} for rid in ("a-west-billing", "a-west-docs")]}
        payload = b"// file: billing/src/app.txt\nfirst\n// file: src/app.txt\nsecond\n"
        with patch.object(driver, "copy_from_button", return_value=payload):
            driver.attempt_collision(self.native, self.sessions, self.manifest, step, {}, self.root, self.root, .001, [], self.record, self.root)

    def test_collision_requires_both_confirmed_mappings_and_specific_probe(self):
        self.collide()
        self.assertEqual(self.record["clipboard"]["collisionMapped"], ["billing", "src"])
        self.assertIn("paste-map-keep:src", self.controls)
        self.assertEqual(len(set(self.record["clipboard"]["canonicalDestinationFiles"])), 1)
        self.assertEqual(self.record["clipboard"]["refusal"], "[APP:PASTE_PLAN_REFUSED: reason=target_collision]")

    def test_first_mapping_clipboard_error_never_passes(self):
        self.mapping_error = "[APP:PASTE_ERR: clipboard]"
        with self.assertRaises(driver.DriverError):
            self.collide()
        self.assertNotEqual(self.record["status"], "passed")

    def test_second_mapping_generic_plan_error_never_passes(self):
        self.error_prefix = "src"
        self.mapping_error = "[APP:PASTE_ERR: paste_err_plan]"
        with self.assertRaises(driver.DriverError):
            self.collide()
        self.assertNotEqual(self.record["status"], "passed")

    def test_zero_items_without_collision_probe_never_passes(self):
        self.emit_collision = False
        with self.assertRaises(driver.DriverError):
            self.collide()

    def test_collision_mapping_wrong_canonical_destination_never_passes(self):
        self.wrong_dest = True
        with self.assertRaises(driver.DriverError):
            self.collide()

    def test_ambiguous_real_ids_expand_source_and_require_fresh_events(self):
        self.ambiguous = True
        driver.block_ambiguous(self.native, self.sessions, self.manifest, {}, self.root, self.root, .001, [], self.record, self.root)
        self.assertEqual(self.record["status"], "passed")
        self.assertEqual(self.record["clipboard"]["candidateCount"], 2)
        self.assertEqual(self.controls.count("tree-row:src"), 2)

    def test_ambiguous_missing_second_candidate_never_passes(self):
        self.ambiguous = self.only_one_candidate = True
        with self.assertRaises(driver.DriverError):
            driver.block_ambiguous(self.native, self.sessions, self.manifest, {}, self.root, self.root, .001, [], self.record, self.root)

    def test_stale_basket_event_cannot_confirm_source_selection(self):
        self.sessions["a"].add("[APP:BASKET: n=1]")
        original_click = self.click
        def no_fresh_basket(*args, **kwargs):
            if not args[3].startswith("tree-chk:"):
                original_click(*args, **kwargs)
        with patch.object(driver, "click_control", side_effect=no_fresh_basket), self.assertRaises(driver.DriverError):
            self.collide()

    def missing_destination(self, truncate=False, auto_map=False, changed_clipboard=False):
        for i in range(14):
            self.manifest["repos"].append({"repoId": f"b-extra-{i}", "basename": f"extra-{i}", "relativePath": f"machine-b/extra-{i}"})
        for repo in self.manifest["repos"]:
            (self.root / repo["relativePath"]).mkdir(parents=True)
        step = {"destRepoId": "b-does-not-exist", "sourceRepoId": "a-west-billing", "sourcePath": "src/app.txt"}
        def preview(native, session, win, timeout, trace):
            roots = [self.root / repo["relativePath"] for repo in self.manifest["repos"] if repo["repoId"].startswith("b-")]
            for idx, root in enumerate(roots[:-1] if truncate else roots):
                session.add(f"[APP:PASTE_MAP_CANDIDATE: prefix=billing idx={idx} path={root}]")
            session.add("[APP:PASTE_PREVIEW: items=0 dest=/tmp mapping=false]")
            if auto_map:
                session.add("[APP:PASTE_MAPPED: prefix=billing dest=/tmp/b-does-not-exist items=1]")
        self.sessions["b"].set_clipboard(b"changed" if changed_clipboard else b"payload")
        with patch.object(driver, "paste_preview", side_effect=preview):
            driver.block_missing_dest(self.native, self.sessions, self.manifest, step, {}, self.root, self.root, .001, [], self.record, self.root)

    def test_missing_destination_proves_whitelist_prevention_only(self):
        self.missing_destination()
        self.assertTrue(driver.valid_whitelist_prevention(self.record["clipboard"]))
        self.assertFalse(self.record["clipboard"]["originalMappedIdSubmitted"])
        self.assertEqual(self.record["clipboard"]["unresolvedPrefix"], "billing")
        self.assertEqual(len(self.record["clipboard"]["candidateCanonicalRoots"]), 15)

    def test_missing_destination_truncated_candidates_fail(self):
        with self.assertRaises(driver.DriverError):
            self.missing_destination(truncate=True)

    def test_missing_destination_automatic_mapping_fails(self):
        with self.assertRaises(driver.DriverError):
            self.missing_destination(auto_map=True)

    def test_missing_destination_missing_keyboard_refusal_fails(self):
        self.sessions["b"].key_result = None
        with self.assertRaises(driver.DriverError):
            self.missing_destination()

    def test_missing_destination_changed_clipboard_fails(self):
        with self.assertRaises(driver.DriverError):
            self.missing_destination(changed_clipboard=True)


class WhitelistValidationTests(unittest.TestCase):
    def test_incomplete_or_invalid_whitelist_evidence_cannot_pass(self):
        mutations = [
            lambda c: c.pop("candidateCanonicalRoots"),
            lambda c: c["candidateCanonicalRoots"].pop(),
            lambda c: c.update(originalMappedIdSubmitted=True),
            lambda c: c.update(automaticMappingObserved=True),
            lambda c: c.update(refusal="[APP:PASTE_ERR: clipboard]"),
            lambda c: c.pop("disabledApplyClicked"),
            lambda c: c.update(readbackSha256="b" * 64),
            lambda c: c["candidateRepoIds"].__setitem__(0, "b-does-not-exist"),
            lambda c: c["candidateCanonicalRoots"].__setitem__(0, "/tmp/b-does-not-exist"),
        ]
        for mutate in mutations:
            report = _valid_report()
            step = next(s for s in report["steps"] if s["id"] == "neg-mapping-missing-destination")
            mutate(step["clipboard"])
            with self.subTest(mutation=mutate):
                self.assertNotEqual(driver.functional_exit_code(report), 0)

    def test_writes_fail_both_no_write_oracle_and_report_validator(self):
        report = _valid_report()
        step = next(s for s in report["steps"] if s["id"] == "neg-mapping-missing-destination")
        step["compare"]["applied"] = "changed"
        self.assertNotEqual(driver.functional_exit_code(report), 0)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            def changed_snapshot(fixture, path):
                payload = {"repos": [{"content": "after-write"}]}
                driver.write_json(path, payload)
                return payload
            with patch.object(driver, "snapshot_file", side_effect=changed_snapshot), self.assertRaisesRegex(driver.DriverError, "changed the baseline"):
                driver.assert_negative_unchanged(root, {"id": "neg-mapping-missing-destination"}, root, driver.blank_step("neg-mapping-missing-destination"), {"repos": [{"content": "before"}]})


class ReportOutcomeTests(unittest.TestCase):
    def test_positive_clipboard_hash_mismatch_fails_gate(self):
        report = _valid_report()
        report["steps"][0]["clipboard"]["readback"]["sha256"] = "b" * 64
        self.assertNotEqual(driver.functional_exit_code(report), 0)

    def test_step_kind_cannot_bypass_its_required_outcome(self):
        for sid in driver.REQUIRED_STEP_IDS:
            report = _valid_report()
            step = next(s for s in report["steps"] if s["id"] == sid)
            step["kind"] = "positive-file" if sid.startswith(("neg-", "commit-")) else "negative"
            with self.subTest(step=sid):
                self.assertNotEqual(driver.functional_exit_code(report), 0)

    def test_case_specific_outcome_is_required_not_a_generic_refusal(self):
        for sid, field in (("neg-stale-target", "apply"), ("neg-cancel", "cancel"), ("neg-overwrite-unauthorized", "apply")):
            for value in (None, "[APP:PASTE_ERR: clipboard]", "[APP:PASTE_DONE: overwritten=1]"):
                report = _valid_report()
                step = next(s for s in report["steps"] if s["id"] == sid)
                step["clipboard"][field] = value
                with self.subTest(step=sid, value=value):
                    self.assertNotEqual(driver.functional_exit_code(report), 0)

    def test_run_steps_emits_the_hashes_required_by_its_own_validator(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = {
                "native": None, "manifest": {"repos": [], "steps": [{"id": sid} for sid in driver.REQUIRED_STEP_IDS]},
                "output": tmp, "timeout": 1, "phase": "all", "binary": {"path": "/binary", "sha256": "a" * 64},
                "fixture": "/fixture", "dataset_hash": "b" * 64, "helpers": {}, "step_ids": driver.REQUIRED_STEP_IDS,
            }
            with patch.object(driver, "run_one", side_effect=lambda native, manifest, step, *args: (_passed_step(step["id"]), False)), patch("builtins.print"):
                report = driver.run_steps(config)
            self.assertEqual(report["binarySha256"], "a" * 64)
            self.assertEqual(report["datasetHash"], "b" * 64)
            self.assertEqual(report["functionalProblems"], [])
            self.assertEqual(report["exitCode"], 0)

    def test_unauthorized_working_file_uses_project_tree_even_when_unchanged(self):
        session = _FlowSession()
        manifest = {"repos": [{"repoId": "a", "basename": "source", "relativePath": "machine-a/source"}, {"repoId": "b", "basename": "dest", "relativePath": "machine-b/dest"}]}
        step = {"operation": {"sourceRepoId": "a", "destRepoId": "b", "sourcePath": "src/app.txt"}}
        native = SimpleNamespace(parse_bounds=lambda lines: {"paste-overwrite:src/app.txt": (0, 0, 10, 10)})
        with tempfile.TemporaryDirectory() as tmp, patch.object(driver, "select_repo"), patch.object(driver, "select_tree_file") as tree, patch.object(driver, "preview_change", side_effect=AssertionError("unchanged file is absent from Changes")), patch.object(driver, "capture_checked", return_value={"png": "shot.png"}), patch.object(driver, "copy_from_button", return_value=b"payload"), patch.object(driver, "transfer_os_clipboard"), patch.object(driver, "paste_preview", return_value="preview"), patch.object(driver, "apply_or_cancel", return_value="[APP:PASTE_DONE: created=0 overwritten=0 skipped=1]"):
            record = driver.blank_step("neg-overwrite-unauthorized")
            driver.run_unauthorized(native, {"a": session, "b": session}, manifest, step, {}, Path(tmp), Path(tmp), 1, [], record, Path(tmp))
            tree.assert_called_once()
            self.assertEqual(record["status"], "passed")


class StaleSourceFlowTests(unittest.TestCase):
    def run_flow(self, failure="[APP:COPY_FAILED: stale_source]", mutate_clipboard=False, ready=True, idle=True):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        source = root / "machine-a/west/billing/transfer/working.txt"
        source.parent.mkdir(parents=True)
        source.write_bytes(b"original")
        hold = root / "export_hold.signal"
        session = _FlowSession()
        record = driver.blank_step("neg-stale-source")
        manifest = {"repos": [{"repoId": "a-west-billing", "basename": "billing", "relativePath": "machine-a/west/billing"}]}
        step = {"preview": {"repoId": "a-west-billing", "path": "transfer/working.txt"}}
        events = []
        original_wait = session.wait_line
        def wait(predicate, start=0, timeout=1):
            if not hold.exists() and ready:
                events.append("released")
                session.add(failure)
                if idle:
                    session.add("[APP:COPY_IDLE]")
                if mutate_clipboard:
                    session.set_clipboard(b"unexpected payload")
            return original_wait(predicate, start, timeout)
        session.wait_line = wait
        def click(*args, **kw):
            self.assertTrue(hold.exists())
            self.assertEqual(source.read_bytes(), b"original")
            events.append("plan-captured")
            if ready:
                session.add("[APP:EXPORT_PLAN_READY: files=1]")
        def snapshot(fixture, path):
            self.assertTrue(hold.exists())
            self.assertEqual(source.read_bytes(), b"original\nSTALE-SOURCE\n")
            events.append("post-mutation-baseline")
            driver.write_json(path, {"repos": [{"bytes": source.read_text()}]})
        with patch.object(driver, "select_repo"), patch.object(driver, "preview_change"), patch.object(driver, "capture_checked", return_value={"png": "shot.png"}), patch.object(driver, "click_control", side_effect=click), patch.object(driver, "snapshot_file", side_effect=snapshot):
            try:
                driver.run_stale_source(None, {"a": session}, manifest, step, {}, root, root, .001, [], record, root)
            finally:
                self.assertFalse(hold.exists())
        return record, events

    def test_plan_capture_precedes_mutation_baseline_and_release(self):
        record, events = self.run_flow()
        self.assertEqual(events[:3], ["plan-captured", "post-mutation-baseline", "released"])
        self.assertTrue(record["clipboard"]["clipboardMatchesSentinel"])
        self.assertIn("preAction", record["snapshots"])

    def test_unrelated_refusal_or_clipboard_write_never_passes(self):
        for failure, changed in (("[APP:COPY_FAILED: revalidate]", False), ("[APP:COPY_DONE: copied=1]", False), ("[APP:COPY_FAILED: stale_source]", True)):
            with self.subTest(failure=failure, changed=changed), self.assertRaises(driver.DriverError):
                self.run_flow(failure=failure, mutate_clipboard=changed)

    def test_missing_plan_probe_fails_and_releases_hold(self):
        with self.assertRaises(driver.DriverError):
            self.run_flow(ready=False)

    def test_missing_idle_fails_after_stale_source_refusal(self):
        with self.assertRaises(driver.DriverError):
            self.run_flow(idle=False)


class SessionEnvironmentTests(unittest.TestCase):
    def test_successful_constructor_restores_environment_before_startup_wait(self):
        key = "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE"
        old = os.environ.get(key)
        with tempfile.TemporaryDirectory() as tmp:
            session = _FlowSession(["[APP:WINDOW_READY]"])
            session.sampler_ready_file = str(Path(tmp) / "ready")
            def wait_app(release, timeout):
                self.assertEqual(os.environ.get(key), old)
                ident = {"pid": 123, "starttime": 456, "exe": "/bin/python3", "comm": "python3"}
                release(ident)
                return ident
            session.wait_app = wait_app
            def constructor(binary, workspace, mode, run_dir, e2e):
                self.assertTrue(driver._ENV_LOCK.locked())
                self.assertEqual(os.environ[key], "temporary")
                return session
            with patch.object(driver.os, "readlink", return_value="/bin/app"):
                native = SimpleNamespace(NativeSession=constructor, identity=lambda pid: {"pid": pid, "starttime": 456, "exe": "/bin/app", "comm": "snip-app"})
                result = driver.open_session(native, Path("/bin/app"), Path(tmp), Path(tmp) / "run", 1, {key: "temporary"})
            self.assertIs(result, session)
            self.assertEqual(session.app, {"pid": 123, "starttime": 456, "exe": "/bin/app", "comm": "snip-app"})
            self.assertEqual(os.environ.get(key), old)
            self.assertEqual(Path(session.sampler_ready_file).read_text(), "functional-only\n")

    def test_ready_identity_rejects_reused_pid_or_wrong_executable(self):
        for changed in ({"starttime": 457}, {"pid": 124}, {"exe": "/bin/other"}, {"starttime": None}):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as tmp:
                session = _FlowSession(["[APP:WINDOW_READY]"])
                session.wait_app = lambda release, timeout: {"pid": 123, "starttime": 456}
                session.stop = lambda: []
                ident = {"pid": 123, "starttime": 456, "exe": "/bin/app", "comm": "snip-app", **changed}
                native = SimpleNamespace(NativeSession=lambda *args: session, identity=lambda pid: ident)
                with patch.object(driver.os, "readlink", return_value="/bin/app"), self.assertRaises(driver.DriverError):
                    driver.open_session(native, Path("/bin/app"), Path(tmp), Path(tmp) / "run", 1)

    def test_constructor_restores_only_requested_keys_even_on_failure(self):
        key = "SNIP_NATIVE_E2E_EXPORT_HOLD_FILE"
        unrelated = "SNIP_COLLAB_TEST_UNRELATED"
        for old in (None, "previous"):
            with self.subTest(old=old), tempfile.TemporaryDirectory() as tmp:
                saved = {k: os.environ.get(k) for k in (key, unrelated)}
                try:
                    if old is None:
                        os.environ.pop(key, None)
                    else:
                        os.environ[key] = old
                    def constructor(binary, workspace, mode, run_dir, e2e):
                        self.assertTrue(driver._ENV_LOCK.locked())
                        self.assertEqual(os.environ[key], "temporary")
                        os.environ[unrelated] = "constructor-update"
                        raise RuntimeError("constructor failed")
                    native = SimpleNamespace(NativeSession=constructor)
                    with self.assertRaisesRegex(RuntimeError, "constructor failed"):
                        driver.open_session(native, Path("/bin/app"), Path(tmp), Path(tmp) / "run", 1, {key: "temporary"})
                    self.assertEqual(os.environ.get(key), old)
                    self.assertEqual(os.environ[unrelated], "constructor-update")
                    def other_constructor(*args):
                        self.assertTrue(driver._ENV_LOCK.locked())
                        self.assertEqual(os.environ.get(key), old)
                        raise RuntimeError("second constructor")
                    with self.assertRaisesRegex(RuntimeError, "second constructor"):
                        driver.open_session(SimpleNamespace(NativeSession=other_constructor), Path("/bin/app"), Path(tmp), Path(tmp) / "other", 1)
                finally:
                    for name, value in saved.items():
                        if value is None:
                            os.environ.pop(name, None)
                        else:
                            os.environ[name] = value


class DiscontinuousFlowTests(unittest.TestCase):
    def test_real_copy_refusal_keeps_the_same_sentinel_and_selects_actual_tips(self):
        session = _FlowSession()
        session.click_result = "[APP:COPY_COMMITS_ERR: commits are not contiguous: following first parents back from tip]"
        native = SimpleNamespace(require_control=lambda *a: (0, 0, 10, 10), assert_on_window=lambda *a: None)
        manifest = {"repos": [{"repoId": "a-west-billing", "basename": "billing", "relativePath": "machine-a/west/billing"}]}
        step = {"selection": {"baseOid": "base", "tipOid": "tip", "oids": ["different", "tip"]}}
        selected = []
        record = driver.blank_step("neg-noncontiguous-tips")
        def shift(*args):
            selected.append(args[4])
            session.add("[APP:RANGE: commits=3]")
        with patch.object(driver, "select_repo"), patch.object(driver, "capture_checked", return_value={"png": "shot.png"}), patch.object(driver, "click_commit_row", side_effect=lambda *a: selected.append(a[4])), patch.object(driver, "shift_click_commit", side_effect=shift):
            driver.refuse_discontinuous(native, {"a": session}, manifest, step, {}, Path("/tmp"), .001, [], record, Path("/tmp"))
        self.assertEqual(selected, ["base", "tip"])
        self.assertEqual(record["clipboard"]["sentinelSha256"], driver.sha256_bytes(session.read_clipboard()))
        self.assertTrue(record["clipboard"]["clipboardMatchesSentinel"])

    def test_no_selection_is_not_discontinuous_proof(self):
        session = _FlowSession()
        manifest = {"repos": [{"repoId": "a-west-billing", "basename": "billing", "relativePath": "machine-a/west/billing"}]}
        step = {"selection": {"baseOid": "base", "tipOid": "tip"}}
        def shift(*args):
            session.add("[APP:RANGE: commits=2]")
        with patch.object(driver, "select_repo"), patch.object(driver, "capture_checked", return_value={"png": "shot.png"}), patch.object(driver, "click_commit_row"), patch.object(driver, "shift_click_commit", side_effect=shift), patch.object(driver, "copy_commits", side_effect=driver.UiRefusal("[APP:COPY_COMMITS_REFUSED: no_selection]")):
            with self.assertRaisesRegex(driver.DriverError, "does not prove discontinuous"):
                driver.refuse_discontinuous(None, {"a": session}, manifest, step, {}, Path("/tmp"), .001, [], driver.blank_step("neg-noncontiguous-tips"), Path("/tmp"))


if __name__ == "__main__":
    unittest.main()
