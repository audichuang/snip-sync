#!/usr/bin/env python3
"""Contract regressions for bench_native_memory.py helpers, bounds parsing, X11 scrolling, and oracles.

Tests:
1. lavapipe ICD detection (multiarch / portable discovery).
2. Control bounds parsing and CTRL_GONE retirement.
3. xwininfo geometry and map state extraction.
4. Repo group expansion in the Changes list.
5. Visible-in scroll direction with edge tolerance.
6. Left tool window viewport derivation.
7. Repo state validation against independent git oracle.
8. ClipCode payload extraction and byte-exact verification against disk files.
"""

from __future__ import annotations

import gc
import hashlib
import inspect
import io
import json
import shlex
import os
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import warnings
from contextlib import redirect_stderr
from typing import Any
import unittest
from unittest import mock

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from bench_native_memory import (  # noqa: E402
    APPLICATION_HISTORY_PAGE_LENGTH,
    DEFAULT_PROFILES,
    NativeBenchError,
    NativeSession,
    assert_copied_payload,
    assert_nothing_copied,
    check_repo_state,
    check_native_matched,
    e2e_scaled,
    click_repo,
    open_repo_name,
    change_row_group,
    choose_copy_target,
    copy_explicit_selection,
    expand_change_dirs,
    extract_clipcode_file_bytes,
    extract_clipcode_file_content,
    lavapipe_icd,
    left_viewport,
    main as native_main,
    parse_bounds,
    parse_repo_select,
    parse_xwininfo,
    repo_oracle,
    require_control,
    run_profile,
    select_native_matched,
    scroll_into_view,
    source_file_bytes,
    source_rows,
    visible_in,
    write_preexec_launcher,
)
from bench_native_memory import BenchError, MATCHED_REF, MATCHED_SENTINEL, load_build_receipt  # noqa: E402
from memory_harness import measure_single_profile  # noqa: E402


def require_or_skip(cond: bool, reason: str) -> None:
    if cond:
        return
    assert os.environ.get("SNIP_REQUIRE_ALL_TESTS") is None, reason
    raise unittest.SkipTest(reason)


class TestE2eScaled(unittest.TestCase):
    """The Xvfb display-number wait gave up at a fixed 60 s while a loaded machine was
    still probing displays; Xvfb then died with "Cannot write display number"."""

    def scaled(self, value: str | None) -> float:
        env = {} if value is None else {"SNIP_E2E_TIMEOUT_SCALE": value}
        with mock.patch.dict(os.environ, env, clear=False):
            if value is None:
                os.environ.pop("SNIP_E2E_TIMEOUT_SCALE", None)
            return e2e_scaled(60.0)

    def test_scale_stretches_the_wait(self) -> None:
        self.assertEqual(self.scaled("2"), 120.0)
        self.assertEqual(self.scaled("2.5"), 150.0)

    def test_unusable_scale_keeps_the_wait(self) -> None:
        for value in (None, "", "fast", "0.5", "0", "-3", "inf", "nan"):
            with self.subTest(value=value):
                self.assertEqual(self.scaled(value), 60.0)


class TestLavapipeIcdDiscovery(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_finds_standard_lvp_json(self) -> None:
        standard = os.path.join(self.tmp.name, "lvp_icd.json")
        with open(standard, "w") as f:
            f.write("{}")
        self.assertEqual(lavapipe_icd(self.tmp.name), standard)

    def test_finds_arch_specific_lvp_json(self) -> None:
        arch = os.path.join(self.tmp.name, "lvp_icd.x86_64.json")
        with open(arch, "w") as f:
            f.write("{}")
        self.assertEqual(lavapipe_icd(self.tmp.name, machine="x86_64"), arch)

    def test_finds_any_lvp_icd_variant(self) -> None:
        custom = os.path.join(self.tmp.name, "lvp_icd.aarch64.json")
        with open(custom, "w") as f:
            f.write("{}")
        self.assertEqual(lavapipe_icd(self.tmp.name, machine="riscv64"), custom)

    def test_missing_raises_error(self) -> None:
        with self.assertRaises(NativeBenchError) as cm:
            lavapipe_icd(self.tmp.name)
        self.assertIn("no lavapipe ICD", str(cm.exception))


class TestOpenRepoName(unittest.TestCase):
    def test_last_selection_until_a_workspace_change(self) -> None:
        a = "[APP:REPO_SELECTING: 0 (repo-01) root=/w/repo-01]"
        b = "[APP:REPO_SELECTING: 1 (repo-02) root=/w/repo-02]"
        self.assertEqual(open_repo_name([a, b]), "repo-02")
        self.assertEqual(open_repo_name([a, b, "[APP:WORKSPACE: state=closed generation=2]"]), None)
        self.assertIsNone(open_repo_name([]))


class TestBoundsAndGone(unittest.TestCase):
    def test_parse_single_and_update(self) -> None:
        lines = [
            "[APP:CTRL_BOUNDS: id=btn-paste x=777 y=8 w=91 h=22]",
            "[APP:CTRL_BOUNDS: id=repo-row:repo-a x=36 y=66 w=280 h=24]",
        ]
        bounds = parse_bounds(lines)
        self.assertEqual(bounds["btn-paste"], (777, 8, 91, 22))
        self.assertEqual(bounds["repo-row:repo-a"], (36, 66, 280, 24))

        # Position updates on new report
        lines.append("[APP:CTRL_BOUNDS: id=repo-row:repo-a x=36 y=90 w=280 h=24]")
        updated = parse_bounds(lines)
        self.assertEqual(updated["repo-row:repo-a"], (36, 90, 280, 24))

    def test_parse_removes_on_ctrl_gone(self) -> None:
        lines = [
            "[APP:CTRL_BOUNDS: id=ref:refs/tags/v0.1.0 x=36 y=100 w=100 h=20]",
            "[APP:CTRL_GONE: id=ref:refs/tags/v0.1.0]",
        ]
        bounds = parse_bounds(lines)
        self.assertNotIn("ref:refs/tags/v0.1.0", bounds)


class TestParseXwininfo(unittest.TestCase):
    def test_parse_valid_xwininfo(self) -> None:
        sample = """
xwininfo: Window id: 0x200001 "snip-desktop-native"

  Absolute upper-left X:  102
  Absolute upper-left Y:  90
  Relative upper-left X:  0
  Relative upper-left Y:  0
  Width: 1080
  Height: 720
  Depth: 24
  Visual: 0x20
  Visual Class: TrueColor
  Border width: 0
  Class: InputOutput
  Colormap: 0x20 (installed)
  Bit Gravity State: NorthWestGravity
  Window Gravity State: NorthWestGravity
  Backing Store: NotUseful
  Save Under: No
  Map State: IsViewable
  Override Redirect State: No
  Corners:  +102+90  -98+90  -98-90  +102-90
  -geometry 1080x720+102+90
"""
        geo = parse_xwininfo(sample)
        self.assertEqual(geo["x"], 102)
        self.assertEqual(geo["y"], 90)
        self.assertEqual(geo["width"], 1080)
        self.assertEqual(geo["height"], 720)
        self.assertEqual(geo["mapState"], "IsViewable")

    def test_missing_field_raises(self) -> None:
        malformed = "Absolute upper-left X: 100\nWidth: 500\n"
        with self.assertRaises(NativeBenchError):
            parse_xwininfo(malformed)


class TestVisibleIn(unittest.TestCase):
    def setUp(self) -> None:
        # Viewport: y=100, height=400 (visible range [100, 500])
        self.vp = (36, 100, 280, 400)

    def test_inside_viewport(self) -> None:
        row = (36, 200, 280, 24)  # [200, 224] inside [100, 500]
        self.assertEqual(visible_in(row, self.vp), 0)

    def test_above_viewport(self) -> None:
        row = (36, 80, 280, 24)  # [80, 104] partially above 100
        self.assertEqual(visible_in(row, self.vp), -1)

    def test_below_viewport(self) -> None:
        row = (36, 490, 280, 24)  # [490, 514] extends past 500
        self.assertEqual(visible_in(row, self.vp), 1)

    def test_edge_tolerance_prevents_jitter(self) -> None:
        # Exactly at edge (100) or 1px outside tolerance is treated as inside (0)
        row_top = (36, 99, 280, 24)
        self.assertEqual(visible_in(row_top, self.vp), 0)
        row_bottom = (36, 477, 280, 24)  # 477 + 24 = 501 (1px beyond 500)
        self.assertEqual(visible_in(row_bottom, self.vp), 0)


class TestLeftViewport(unittest.TestCase):
    def test_computes_left_viewport_geometry(self) -> None:
        lines = [
            "[APP:CTRL_BOUNDS: id=left-list x=36 y=66 w=280 h=398]",
            "[APP:CTRL_BOUNDS: id=splitter-bottom x=36 y=464 w=280 h=4]",
        ]
        vp = left_viewport(lines)
        self.assertEqual(vp, (36, 66, 280, 398))

    def test_missing_bounds_raises(self) -> None:
        with self.assertRaises(NativeBenchError):
            left_viewport([])

    def test_left_scroll_is_not_accepted(self) -> None:
        lines = ["[APP:CTRL_BOUNDS: id=left-scroll x=36 y=66 w=280 h=800]"]
        with self.assertRaises(NativeBenchError) as cm:
            left_viewport(lines)
        self.assertIn("left-list", str(cm.exception))
        self.assertIn("left-scroll", str(cm.exception))


class TestSourceAwareControls(unittest.TestCase):
    def test_repo_select_keeps_name_when_root_follows(self) -> None:
        idx, name = parse_repo_select(
            "[APP:REPO_SELECTING: 1 (repo-02-api) root=/tmp/snip/repo-02-api]"
        )
        self.assertEqual((idx, name), (1, "repo-02-api"))

    def test_path_only_change_row_is_not_accepted(self) -> None:
        lines = ["[APP:CTRL_BOUNDS: id=change-row:both.txt x=1 y=2 w=10 h=10]"]
        with self.assertRaises(NativeBenchError) as cm:
            require_control(lines, "change-row:staged:both.txt")
        self.assertIn("not accepted", str(cm.exception))
        self.assertIn("change-row:both.txt", str(cm.exception))


class TestRepoStateCheck(unittest.TestCase):
    def setUp(self) -> None:
        self.oracle = {
            "name": "repo-01-core",
            "sourceRows": [
                {"path": "both.txt", "source": "staged", "deleted": False, "conflict": False},
                {"path": "both.txt", "source": "unstaged", "deleted": False, "conflict": False},
            ],
            "historyRowsExpected": APPLICATION_HISTORY_PAGE_LENGTH,
            "historyFirst": "abcdef0",
        }

    def _lines(self, files: int, commits: int = APPLICATION_HISTORY_PAGE_LENGTH) -> list[str]:
        return [
            f"[APP:REPO_LOADED: repo-01-core files={files}]",
            f"[APP:GRAPH_LOADED: commits={commits}]",
            "[APP:E2E_LOG: mode=graph n=50 first=abcdef0 page=1]",
            "[APP:PREVIEW_LOADED: both.txt]",
        ]

    def test_matching_state_counts_source_rows(self) -> None:
        state = check_repo_state(self._lines(2), self.oracle)
        self.assertEqual(state["changedFiles"], 2)
        self.assertEqual(state["sourceRows"], 2)
        self.assertEqual(state["distinctPaths"], 1)
        self.assertEqual(state["historyRows"], APPLICATION_HISTORY_PAGE_LENGTH)
        self.assertEqual(state["previewPath"], "both.txt")

    def test_distinct_path_count_is_not_the_oracle(self) -> None:
        with self.assertRaises(NativeBenchError) as cm:
            check_repo_state(self._lines(1), self.oracle)
        text = str(cm.exception)
        self.assertIn("lists 1 source rows", text)
        self.assertIn("2 source rows", text)
        self.assertIn("1 distinct", text)

    def test_mismatch_history_raises(self) -> None:
        with self.assertRaises(NativeBenchError) as cm:
            check_repo_state(self._lines(2, commits=10), self.oracle)
        self.assertIn("history rows [10], expected 50", str(cm.exception))


class TestClipcodePayloadExtraction(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_single_file_delimiter_keeps_the_files_own_newline(self) -> None:
        # Staged fallback omits empty wrappers: file bytes, then one delimiter newline.
        content = b"INDEX_A\n"
        wire = b"// clipcode-root: both-repo\n// file: [MODIFIED] both.txt\n" + content + b"\n"
        self.assertEqual(extract_clipcode_file_bytes(wire, "both.txt", expected_root="both-repo"), content)

    def test_unstaged_empty_wrappers_keep_the_worktree_newline(self) -> None:
        # Captured from the immutable D3 binary: empty pre-text is the blank line
        # after the root, empty post-text is one extra newline after the delimiter.
        content = b"WORK_B\n"
        wire = b"// clipcode-root: both-repo\n\n// file: [MODIFIED] both.txt\n" + content + b"\n\n"
        self.assertEqual(extract_clipcode_file_bytes(wire, "both.txt", expected_root="both-repo"), content)

    def test_extract_bytes_crlf_and_multiple_blank_lines(self) -> None:
        raw_content = b"line1\r\nline2\r\n\r\n"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: [MODIFIED] src/file.txt\n"
            + raw_content +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "src/file.txt", expected_root="repo-01-core")
        self.assertIsNotNone(extracted)
        self.assertEqual(extracted, raw_content)

        # Write to disk and verify raw byte equality
        disk_path = os.path.join(self.tmp.name, "file.txt")
        with open(disk_path, "wb") as f:
            f.write(raw_content)
        with open(disk_path, "rb") as f:
            disk_bytes = f.read()
        self.assertEqual(extracted, disk_bytes)
        self.assertEqual(hashlib.sha256(extracted).hexdigest(), hashlib.sha256(disk_bytes).hexdigest())

    def test_extract_bytes_no_trailing_newline(self) -> None:
        raw_content = b"content without trailing newline"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: README.md\n"
            + raw_content +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "README.md", expected_root="repo-01-core")
        self.assertEqual(extracted, raw_content)

    def test_extract_bytes_utf8_bom(self) -> None:
        raw_content = b"\xef\xbb\xbfconst title = 'with BOM';\n"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: [NEW] bom.ts\n"
            + raw_content +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "bom.ts", expected_root="repo-01-core")
        self.assertEqual(extracted, raw_content)
        self.assertTrue(extracted.startswith(b"\xef\xbb\xbf"))

    def test_extract_bytes_non_ascii_unicode(self) -> None:
        raw_content = "這是繁體中文註解\n第二行包含 Unicode: 🚀\n".encode("utf-8")
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: [MODIFIED] doc.md\n"
            + raw_content +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "doc.md", expected_root="repo-01-core")
        self.assertEqual(extracted, raw_content)

    def test_extract_bytes_header_lookalike_unescaped(self) -> None:
        # A file containing lines that look like ClipCode headers
        expected_raw = b"before\n// file: fake/inline.ts\nafter\n"
        escaped_body = b"before\n//clipcode-esc: // file: fake/inline.ts\nafter\n"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: lookalike.ts\n"
            + escaped_body +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "lookalike.ts", expected_root="repo-01-core")
        self.assertEqual(extracted, expected_raw)

    def test_extract_bytes_double_escaped_unescaped(self) -> None:
        expected_raw = b"//clipcode-esc: // file: x.ts\r\n"
        escaped_body = b"//clipcode-esc: //clipcode-esc: // file: x.ts\r\n"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: double.ts\n"
            + escaped_body +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "double.ts", expected_root="repo-01-core")
        self.assertEqual(extracted, expected_raw)

    def test_extract_bytes_clipcode_end_lookalike(self) -> None:
        expected_raw = b"section\n// clipcode-end\r\npost\n"
        escaped_body = b"section\n//clipcode-esc: // clipcode-end\r\npost\n"
        wire = (
            b"// clipcode-root: repo-01-core\n"
            b"// file: marker.ts\n"
            + escaped_body +
            b"\n"
        )
        extracted = extract_clipcode_file_bytes(wire, "marker.ts", expected_root="repo-01-core")
        self.assertEqual(extracted, expected_raw)

    def test_extract_bytes_multi_file_payload(self) -> None:
        f1_content = b"first file\r\nline2\r\n\r\n"
        f2_content = b"second file\nwith no extra blank lines\n"
        wire = (
            b"// clipcode-root: my-multi-repo\n"
            b"// file: [MODIFIED] f1.txt\n"
            + f1_content +
            b"\n\n"
            b"// file: [NEW] f2.txt\n"
            + f2_content +
            b"\n"
        )
        ext1 = extract_clipcode_file_bytes(wire, "f1.txt", expected_root="my-multi-repo")
        ext2 = extract_clipcode_file_bytes(wire, "f2.txt", expected_root="my-multi-repo")
        self.assertEqual(ext1, f1_content)
        self.assertEqual(ext2, f2_content)

    def test_extract_bytes_root_header_mismatch_raises(self) -> None:
        wire = b"// clipcode-root: wrong-repo\n\n// file: a.ts\nconst a = 1;\n\n"
        with self.assertRaises(NativeBenchError) as cm:
            extract_clipcode_file_bytes(wire, "a.ts", expected_root="expected-repo")
        self.assertIn("root header mismatch", str(cm.exception))

    def test_extract_bytes_invalid_utf8_raises(self) -> None:
        bad_wire = b"// clipcode-root: repo\n\xff\xfe not valid utf8"
        with self.assertRaises(NativeBenchError) as cm:
            extract_clipcode_file_bytes(bad_wire, "a.ts")
        self.assertIn("clipboard payload is not valid UTF-8", str(cm.exception))

    def test_extract_bytes_missing_path_returns_none(self) -> None:
        wire = b"// clipcode-root: repo\n\n// file: exists.ts\ncontent\n\n"
        self.assertIsNone(extract_clipcode_file_bytes(wire, "does_not_exist.ts"))

    def test_extract_content_compat_wrapper(self) -> None:
        wire_str = "// clipcode-root: repo\n\n// file: a.ts\nfoo\n"
        content = extract_clipcode_file_content(wire_str, "a.ts")
        self.assertEqual(content, "foo")


def _wire(root: str, path: str, body: bytes, label: bytes = b"") -> bytes:
    tag = label + b" " if label else b""
    return (
        f"// clipcode-root: {root}\n".encode()
        + b"// file: " + tag + path.encode() + b"\n"
        + body
        + b"\n"
    )


class TestCopiedPayloadOracle(unittest.TestCase):
    def test_exact_bytes_and_single_entry(self) -> None:
        body = b"EXPECTED CONTENT\r\n\r\n"
        payload = _wire("my-repo", "fixture/file.txt", body, b"[MODIFIED]")
        got = assert_copied_payload(
            payload, root="my-repo", path="fixture/file.txt", expected=body, copied_count=1,
        )
        self.assertTrue(got["verified"])
        self.assertEqual(got["copiedCount"], 1)
        self.assertEqual(got["paths"], ["fixture/file.txt"])
        self.assertEqual(got["oracleSha256"], hashlib.sha256(body).hexdigest())

    def test_mismatch_raises(self) -> None:
        # Same framing as the historical supervisor case: wrong body, 13 bytes after unescape.
        payload = (
            b"// clipcode-root: my-repo\n\n"
            b"// file: fixture/file.txt\n"
            b"WRONG CONTENT\n"
        )
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(
                payload, root="my-repo", path="fixture/file.txt",
                expected=b"EXPECTED CONTENT\r\n\r\n", copied_count=1,
            )
        text = str(cm.exception)
        self.assertIn("do not match the source oracle", text)
        self.assertIn("extracted 13 bytes", text)
        self.assertIn("oracle 20 bytes", text)

    def test_extra_entry_raises(self) -> None:
        payload = (
            _wire("my-repo", "a.txt", b"one\n")
            + b"// file: b.txt\n" + b"two\n\n"
        )
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(payload, root="my-repo", path="a.txt", expected=b"one\n", copied_count=1)
        self.assertIn("not exactly the selected path", str(cm.exception))

    def test_sentinel_must_be_replaced(self) -> None:
        sentinel = b"SNIP-DRIVER-SENTINEL-abc\n"
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(
                sentinel, root="my-repo", path="a.txt", expected=b"a\n",
                copied_count=1, sentinel=sentinel,
            )
        self.assertIn("sentinel", str(cm.exception))

    def test_empty_clipboard_raises(self) -> None:
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(b"", root="my-repo", path="a.txt", expected=b"a\n", copied_count=1)
        self.assertIn("empty", str(cm.exception))

    def test_clipboard_not_utf8_raises(self) -> None:
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(
                b"\xff\xfe bad", root="my-repo", path="a.txt", expected=b"a\n", copied_count=1,
            )
        self.assertIn("not valid UTF-8", str(cm.exception))

    def test_root_header_mismatch_raises(self) -> None:
        payload = _wire("other", "a.txt", b"a\n")
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(payload, root="my-repo", path="a.txt", expected=b"a\n", copied_count=1)
        self.assertIn("root header", str(cm.exception))

    def test_missing_file_raises(self) -> None:
        payload = _wire("my-repo", "other.txt", b"a\n")
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(payload, root="my-repo", path="a.txt", expected=b"a\n", copied_count=1)
        self.assertIn("not exactly the selected path", str(cm.exception))

    def test_copied_count_must_be_one(self) -> None:
        payload = _wire("my-repo", "a.txt", b"a\n")
        with self.assertRaises(NativeBenchError) as cm:
            assert_copied_payload(payload, root="my-repo", path="a.txt", expected=b"a\n", copied_count=9)
        self.assertIn("copied=9", str(cm.exception))


def _git(repo: str, *args: str) -> None:
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR")}
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    subprocess.check_call(
        ["git", "-c", "user.name=T", "-c", "user.email=t@example.com", "-C", repo, *args],
        env=env, stdout=subprocess.DEVNULL,
    )


class TestIndexAndWorktreeOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = os.path.join(self.tmp.name, "idx-repo")
        os.makedirs(self.repo)
        _git(self.repo, "init", "-q", "-b", "main")
        both = os.path.join(self.repo, "both.txt")
        with open(both, "wb") as f:
            f.write(b"base\n")
        _git(self.repo, "add", "both.txt")
        _git(self.repo, "commit", "-qm", "base")
        with open(both, "wb") as f:
            f.write(b"INDEX_A\n")
        _git(self.repo, "add", "both.txt")
        with open(both, "wb") as f:
            f.write(b"WORK_B\n")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_same_path_is_two_sources_and_staged_bytes_are_the_index(self) -> None:
        rows = source_rows(self.repo)
        both = [row for row in rows if row["path"] == "both.txt"]
        self.assertEqual(len(both), 2)
        self.assertEqual({row["source"] for row in both}, {"staged", "unstaged"})
        staged = next(row for row in both if row["source"] == "staged")
        unstaged = next(row for row in both if row["source"] == "unstaged")
        self.assertEqual(source_file_bytes(self.repo, staged), b"INDEX_A\n")
        self.assertEqual(source_file_bytes(self.repo, unstaged), b"WORK_B\n")
        chosen, data = choose_copy_target(self.repo, rows)
        self.assertEqual(chosen["source"], "staged")
        self.assertEqual(data, b"INDEX_A\n")
        self.assertEqual(len({row["path"] for row in rows}), 1)
        self.assertEqual(len(rows), 2)

    def test_explicit_click_copies_index_bytes_not_the_worktree(self) -> None:
        body = b"INDEX_A\n"
        payload = _wire("idx-repo", "both.txt", body, b"[MODIFIED]")
        lines = [
            "[APP:CTRL_BOUNDS: id=left-list x=0 y=40 w=320 h=400]",
            "[APP:CTRL_BOUNDS: id=change-row:staged:both.txt x=8 y=80 w=280 h=22]",
        ]
        session = _ScriptedCopySession(lines, payload, self.tmp.name)
        win = {"wid": "0x1", "x": 0, "y": 0, "width": 800, "height": 600}
        oracle = {
            "name": "idx-repo",
            "repoPath": self.repo,
            "sourceRows": source_rows(self.repo),
        }
        result = copy_explicit_selection(session, win, oracle)
        self.assertEqual(result["source"], "staged")
        self.assertEqual(result["oracleKind"], "index")
        self.assertTrue(result["worktreeDiffers"])
        self.assertEqual(result["paths"], ["both.txt"])
        self.assertEqual(result["copiedCount"], 1)
        self.assertEqual(session.keys, [])
        # Preview click, right click on the same row, then the menu's Copy.
        self.assertEqual([button for _, button in session.clicks], ["1", "3", "1"])
        self.assertEqual(session.clicks[0][0], session.clicks[1][0])
        self.assertEqual(result["controls"], {"row": "change-row:staged:both.txt", "copy": "menu-item:copy-files"})
        self.assertTrue(result["sentinelReplaced"])

    def test_staged_rename_reports_moved_and_handles_worktree_edit(self) -> None:
        old_path = os.path.join(self.repo, "old.txt")
        with open(old_path, "wb") as f:
            f.write(b"original\n")
        _git(self.repo, "add", "old.txt")
        _git(self.repo, "commit", "-qm", "add old")

        _git(self.repo, "mv", "old.txt", "new.txt")
        rows = source_rows(self.repo)
        new_rows = [r for r in rows if r["path"] == "new.txt"]
        old_rows = [r for r in rows if r["path"] == "old.txt"]
        self.assertEqual(len(old_rows), 0, "old path must not appear in source rows")
        self.assertEqual(len(new_rows), 1)
        self.assertEqual(new_rows[0]["source"], "staged")
        self.assertFalse(new_rows[0]["deleted"])
        self.assertEqual(source_file_bytes(self.repo, new_rows[0]), b"original\n")

        new_path = os.path.join(self.repo, "new.txt")
        with open(new_path, "wb") as f:
            f.write(b"modified worktree\n")
        rows_rm = source_rows(self.repo)
        new_rm = [r for r in rows_rm if r["path"] == "new.txt"]
        self.assertEqual(len(new_rm), 2)
        self.assertEqual({r["source"] for r in new_rm}, {"staged", "unstaged"})
        staged_rm = next(r for r in new_rm if r["source"] == "staged")
        unstaged_rm = next(r for r in new_rm if r["source"] == "unstaged")
        self.assertEqual(source_file_bytes(self.repo, staged_rm), b"original\n")
        self.assertEqual(source_file_bytes(self.repo, unstaged_rm), b"modified worktree\n")

        sp_old = os.path.join(self.repo, "space old.txt")
        with open(sp_old, "wb") as f:
            f.write(b"space content\n")
        _git(self.repo, "add", "space old.txt")
        _git(self.repo, "commit", "-qm", "add space old")
        _git(self.repo, "mv", "space old.txt", "space new.txt")
        rows_sp = source_rows(self.repo)
        sp_new = [r for r in rows_sp if r["path"] == "space new.txt"]
        sp_old_rows = [r for r in rows_sp if r["path"] == "space old.txt"]
        self.assertEqual(len(sp_old_rows), 0)
        self.assertEqual(len(sp_new), 1)
        self.assertEqual(sp_new[0]["source"], "staged")
        self.assertEqual(source_file_bytes(self.repo, sp_new[0]), b"space content\n")

    def test_source_rows_truncated_rename_missing_orig_path_raises(self) -> None:
        truncated_raw = (
            b"2 R. N... 100644 100644 100644 "
            b"97fce6207b29351be348868f869fcd7c088b19c9 "
            b"97fce6207b29351be348868f869fcd7c088b19c9 R100 new.txt\0"
        )
        with mock.patch("bench_native_memory.git", return_value=truncated_raw):
            with self.assertRaises(NativeBenchError) as cm:
                source_rows(self.repo)
            self.assertIn("missing rename origPath", str(cm.exception))


class TestNothingCopied(unittest.TestCase):
    """Loading or switching a repository must not run a Copy."""

    def test_load_logs_without_copy_pass(self) -> None:
        lines = ["[APP:REPO_LOADED: repo-01 files=1]", "[APP:TREE_SELECTED: a.txt]", "[APP:MENU_OPEN: Left items=copy-files]"]
        self.assertEqual(assert_nothing_copied(lines, "opening repo-02"), {"copyEvents": 0, "empty": True})

    def test_any_copy_event_fails(self) -> None:
        for line in ("[APP:COPY_PREP: files=1]", "[APP:COPY_DONE: copied=1]"):
            with self.subTest(line=line), self.assertRaises(NativeBenchError) as caught:
                assert_nothing_copied(["[APP:REPO_LOADED: repo-01 files=1]", line], "opening repo-02")
            self.assertIn(line, str(caught.exception))

    def test_click_repo_checks_its_fresh_slice(self) -> None:
        source = inspect.getsource(click_repo)
        self.assertIn("assert_nothing_copied(s.texts(before)", source)


class ChangeTreeSession:
    """Enough of NativeSession to click Changes directory rows without X11.

    `on_click` maps a clicked control id to the lines the app logs in reply.
    """

    def __init__(
        self,
        lines: list[str],
        on_click: dict[str, list[list[str]]],
        on_wheel: list[list[str]] | None = None,
    ) -> None:
        self.lines = ["[APP:CTRL_BOUNDS: id=left-list x=0 y=40 w=320 h=600]", *lines]
        self.on_click = {key: list(replies) for key, replies in on_click.items()}
        self.on_wheel = list(on_wheel or [])
        self.clicked: list[str] = []
        self.wheels = 0

    def texts(self, start: int = 0) -> list[str]:
        return self.lines[start:]

    def click(self, win: dict[str, Any], bounds: tuple[int, int, int, int]) -> float:
        control = next(key for key, box in parse_bounds(self.lines).items() if box == bounds)
        self.clicked.append(control)
        self.lines.extend(self.on_click[control].pop(0))
        return 1.0

    def wait_line(self, pred: Any, start: int = 0, timeout: float = 8.0) -> tuple[int, float, str]:
        for i, line in enumerate(self.lines[start:], start=start):
            if pred(line):
                return i, 1.0, line
        raise NativeBenchError(f"timed out after {timeout}s")

    def focus(self, wid: str) -> None:
        return None

    def x(self, *args: str, timeout: float = 20.0) -> str:
        if "click" in args:
            self.wheels += 1
            if self.on_wheel:
                self.lines.extend(self.on_wheel.pop(0))
        return ""


def _bounds(control: str, y: int) -> str:
    return f"[APP:CTRL_BOUNDS: id={control} x=8 y={y} w=280 h=22]"


class TestChangeDirectoryExpansion(unittest.TestCase):
    """Changes groups files by directory, and directories start collapsed."""

    WIN = {"wid": "0x1", "x": 0, "y": 0, "width": 800, "height": 700}
    ROW = "change-row:untracked:src/main/java/pkg/App.java"

    def test_untracked_rows_sit_in_unstaged(self) -> None:
        self.assertEqual(change_row_group("change-row:untracked:a/b.txt"), "unstaged")
        self.assertEqual(change_row_group("change-row:staged:a/b.txt"), "staged")

    def test_opens_each_collapsed_ancestor_down_to_the_row(self) -> None:
        src = "change-dir:unstaged:repo:src"
        chain = "change-dir:unstaged:repo:src/main/java/pkg"
        session = ChangeTreeSession(
            [_bounds(src, 80), _bounds("change-dir:unstaged:repo:docs", 300)],
            {
                src: [[
                    "[APP:CHANGE_DIR_COLLAPSED: unstaged repo src files=2 collapsed=false]",
                    _bounds(chain, 104),
                    _bounds("change-dir:unstaged:repo:src/test", 152),
                ]],
                chain: [[
                    "[APP:CHANGE_DIR_COLLAPSED: unstaged repo src/main/java/pkg files=1 collapsed=false]",
                    _bounds(self.ROW, 128),
                ]],
            },
        )
        opened = expand_change_dirs(session, self.WIN, self.ROW, "repo", timeout=1)
        self.assertEqual(opened, ["src", "src/main/java/pkg"])
        self.assertEqual(session.clicked, [src, chain])

    def test_reopens_a_directory_whose_children_were_out_of_view(self) -> None:
        src = "change-dir:unstaged:repo:src"
        session = ChangeTreeSession(
            [_bounds(src, 80)],
            {
                src: [
                    ["[APP:CHANGE_DIR_COLLAPSED: unstaged repo src files=1 collapsed=true]"],
                    [
                        "[APP:CHANGE_DIR_COLLAPSED: unstaged repo src files=1 collapsed=false]",
                        _bounds("change-row:untracked:src/a.txt", 104),
                    ],
                ],
            },
        )
        opened = expand_change_dirs(session, self.WIN, "change-row:untracked:src/a.txt", "repo", timeout=1)
        self.assertEqual(opened, ["src"])
        self.assertEqual(session.clicked, [src, src])

    def test_sweeps_the_list_from_the_top_to_find_its_folders(self) -> None:
        node = "change-repo:unstaged:repo"
        src = "change-dir:unstaged:repo:src"
        session = ChangeTreeSession(
            [_bounds(node, 560)],
            {
                src: [[
                    "[APP:CHANGE_DIR_COLLAPSED: unstaged repo src files=1 collapsed=false]",
                    _bounds("change-row:unstaged:src/a.txt", 560),
                ]],
            },
            # Two still wheel-ups find the top; the second wheel-down reveals `src`.
            on_wheel=[[], [], [], [_bounds(node, 500), _bounds(src, 524)]],
        )
        opened = expand_change_dirs(session, self.WIN, "change-row:unstaged:src/a.txt", "repo", timeout=1)
        self.assertEqual(opened, ["src"])
        self.assertEqual(session.wheels, 4)
        self.assertEqual(session.clicked, [src])

    def test_visible_row_and_root_files_need_no_click(self) -> None:
        session = ChangeTreeSession([_bounds(self.ROW, 80)], {})
        self.assertEqual(expand_change_dirs(session, self.WIN, self.ROW, "repo", timeout=1), [])
        session = ChangeTreeSession([], {})
        self.assertEqual(expand_change_dirs(session, self.WIN, "change-row:staged:a.txt", "repo", timeout=1), [])
        self.assertEqual(session.clicked, [])


class _ScriptedCopySession:
    """Enough of NativeSession to drive one explicit copy without X11."""

    def __init__(self, lines: list[str], payload: bytes, run_dir: str) -> None:
        self.lines = list(lines)
        self.payload = payload
        self.run_dir = run_dir
        self.clip = b""
        self.clicks: list[tuple[tuple[int, int, int, int], str]] = []
        self.keys: list[str] = []
        self.copied = False

    def texts(self, start: int = 0) -> list[str]:
        return self.lines[start:]

    def click(self, win: dict[str, Any], bounds: tuple[int, int, int, int], button: str = "1") -> float:
        self.clicks.append((bounds, button))
        n = len(self.clicks)
        if n == 1:
            self.lines.append("[APP:PREVIEW_LOADED: both.txt]")
            self.lines.append(
                "[APP:E2E_PREVIEW: source=staged_changes rev=- path=both.txt lines=1 fnv=1]"
            )
        elif n == 2 and button == "3":
            self.lines.append("[APP:MENU_OPEN: Left items=copy-files,show-diff,copy-path]")
            self.lines.append("[APP:CTRL_BOUNDS: id=menu-item:copy-files x=20 y=100 w=180 h=22]")
        elif n == 3:
            self.lines.append("[APP:MENU_ACTION: copy-files]")
            self.lines.append("[APP:COPY_PREP: files=1]")
            self.lines.append("[APP:COPY_DONE: copied=1]")
            self.copied = True
        return 1.0

    def wait_line(self, pred: Any, start: int = 0, timeout: float = 8.0) -> tuple[int, float, str]:
        for i, line in enumerate(self.lines[start:], start=start):
            if pred(line):
                return i, 1.0, line
        raise NativeBenchError(f"timed out after {timeout}s")

    def set_clipboard(self, payload: bytes) -> None:
        self.clip = payload

    def read_clipboard(self) -> bytes:
        return self.payload if self.copied else self.clip

    def key(self, wid: str, keys: str) -> float:
        self.keys.append(keys)
        return 1.0

    def focus(self, wid: str) -> None:
        return None

    def x(self, *args: str, timeout: float = 20.0) -> str:
        return ""


class TestDriverEndToEndRegression(unittest.TestCase):
    """End-to-end regression proving that wrong clipboard bytes yield FAILED run with nonzero CLI exit and clean teardown."""

    def test_driver_fails_cleanly_on_corrupted_clipboard_bytes(self) -> None:
        from unittest.mock import MagicMock, patch

        with tempfile.TemporaryDirectory() as tmp_dir:
            run_dir = os.path.join(tmp_dir, "run-01")
            dataset_dir = os.path.join(tmp_dir, "dataset")
            repo_dir = os.path.join(dataset_dir, "repo-01-core")
            os.makedirs(repo_dir)

            # Create a tracked file in repo
            sample_file = os.path.join(repo_dir, "README.md")
            with open(sample_file, "wb") as f:
                f.write(b"# Original expected disk bytes\r\n\r\n")

            # Mock NativeSession
            mock_session = MagicMock()
            mock_session.cmd = ["mock-app"]
            mock_session.isolation = {"xvfb": ":99"}
            mock_session.lines = ["[APP:WINDOW_READY]", "[APP:READY_REPOS: 1]", "[APP:REPO_SELECTING: 0 (repo-01-core)]", "[APP:COPY_DONE: copied=1]"]
            mock_session.texts.return_value = [
                "[APP:REPO_LOADED: repo-01-core files=1]",
                "[APP:GRAPH_LOADED: commits=1]",
                "[APP:PREVIEW_LOADED: README.md]",
            ]
            mock_session.window.return_value = {"wid": "0x123", "x": 0, "y": 0, "width": 800, "height": 600}
            mock_session.app_tree_pids.return_value = []
            mock_session.stop.return_value = []  # Clean cleanup

            # Mock xclip returning WRONG BYTES
            def mock_x_bytes(*args: str, **kwargs: any) -> bytes:
                if args and args[0] == "xclip":
                    return (
                        b"// clipcode-root: repo-01-core\n"
                        b"// file: README.md\n"
                        b"WRONG CORRUPTED BYTES\n"
                    )
                return b""

            mock_session.x_bytes = mock_x_bytes

            def fake_key(wid: str, keys: str) -> float:
                if keys == "ctrl+c":
                    mock_session.lines.append("[APP:COPY_DONE: copied=1]")
                return 1.0

            mock_session.key = fake_key

            def fake_wait_line(pred: any, start: int = 0, timeout: float = 120.0) -> tuple[int, float, str]:
                for i, l in enumerate(mock_session.lines[start:], start=start):
                    if pred(l):
                        return i, 1.0, l
                raise NativeBenchError("timed out")

            mock_session.wait_line = fake_wait_line

            def fake_copy(session: Any, win: dict[str, Any], oracle: dict[str, Any]) -> dict[str, Any]:
                with open(sample_file, "rb") as fh:
                    expected = fh.read()
                return assert_copied_payload(
                    session.x_bytes("xclip", "-selection", "clipboard", "-o"),
                    root="repo-01-core",
                    path="README.md",
                    expected=expected,
                    copied_count=1,
                )

            with patch("bench_native_memory.NativeSession", return_value=mock_session), \
                 patch("bench_native_memory.workspace_repos", return_value=[repo_dir]), \
                 patch("bench_native_memory.repo_oracle", return_value={
                     "name": "repo-01-core",
                     "repoPath": repo_dir,
                     "sourceRows": [{"path": "README.md", "source": "unstaged", "deleted": False, "conflict": False}],
                     "distinctPaths": 1,
                     "historyRowsExpected": APPLICATION_HISTORY_PAGE_LENGTH,
                     "historyFirst": "1234567",
                     "newestCommits": ["1234567", "commit msg"],
                 }), \
                 patch("bench_native_memory.wait_repo_loaded", return_value=(1.0, 1.1)), \
                 patch("bench_native_memory.copy_explicit_selection", side_effect=fake_copy), \
                 patch("bench_native_memory.check_repo_state", return_value={
                     "changedFiles": 1, "historyRows": 1, "previewPath": "README.md",
                     "sourceRows": 1, "distinctPaths": 1,
                 }):

                res = run_profile(
                    "1repo",
                    bin_path="/mock/bin",
                    dataset=dataset_dir,
                    repo=repo_dir,
                    run_dir=run_dir,
                    steady=0.1,
                    interval=0.05,
                    soak_switches=1,
                )

                # 1. Run must be recorded as FAILED
                self.assertEqual(res["status"], "FAILED")
                # 2. Error must report NativeBenchError with mismatch details
                self.assertIn("NativeBenchError: copied bytes for 'README.md' do not match the source oracle", res["error"])
                # 3. Teardown must be cleanly invoked
                mock_session.stop.assert_called_once()
                self.assertEqual(res["cleanupProblems"], [])


class TestMatchedNative(unittest.TestCase):
    def setUp(self) -> None:
        self.oracle = {"sha": "a" * 40, "path": "a.txt", "historyOids": ["a" * 40, "b" * 40],
                       "patch": "hello", "patchSha256": hashlib.sha256(b"hello").hexdigest()}
        self.window = {"width": 1080, "height": 720}
        self.lines = ["[APP:LOCALE: En]", f"[APP:REF_FILTER: {MATCHED_REF}]", "[APP:GRAPH_LOADED: commits=2]",
                      "[APP:E2E_LOG: mode=graph n=2 first=aaaaaaa page=1]",
                      "[APP:CTRL_BOUNDS: id=commit-row:aaaaaaa x=10 y=600 w=600 h=20]",
                      "[APP:CTRL_BOUNDS: id=commit-row:bbbbbbb x=10 y=620 w=600 h=20]",
                      f"[APP:E2E_PREVIEW: source=commit_diff rev={'a' * 40} path=a.txt lines=1 fnv=a430d84680aabd0b]"]

    def test_matches_retained_patch_and_displayed_history(self) -> None:
        observed = check_native_matched(self.lines, self.window, self.oracle, MATCHED_SENTINEL)
        self.assertTrue(observed["retainedPatchMatchedGit"])
        self.assertEqual(observed["displayedHistoryShortOids"], ["aaaaaaa", "bbbbbbb"])

    def test_rejects_oid_path_count_order_patch_and_copy_mismatch(self) -> None:
        for old, new in (("rev=" + "a" * 40, "rev=" + "b" * 40), ("path=a.txt", "path=other.txt"),
                         ("commits=2", "commits=50"), ("y=600", "y=640"),
                         ("lines=1", "lines=2"), ("fnv=a430d84680aabd0b", "fnv=0"),
                         ("source=commit_diff", "source=working_changes"), ("mode=graph", "mode=search"),
                         ("LOCALE: En", "LOCALE: ZhTw")):
            with self.subTest(change=new), self.assertRaises((NativeBenchError, BenchError)):
                check_native_matched([line.replace(old, new) for line in self.lines], self.window, self.oracle, MATCHED_SENTINEL)
        for extra in ("[APP:COPY_PREP: files=1]", "[APP:COPY_DONE: copied=1]"):
            with self.subTest(extra=extra), self.assertRaises(NativeBenchError):
                check_native_matched(self.lines + [extra], self.window, self.oracle, MATCHED_SENTINEL)

    def test_rejects_geometry_and_clipboard_mismatch(self) -> None:
        with self.assertRaisesRegex(BenchError, "width"):
            check_native_matched(self.lines, {**self.window, "width": 1000}, self.oracle, MATCHED_SENTINEL)
        with self.assertRaisesRegex(BenchError, "clipboard"):
            check_native_matched(self.lines, self.window, self.oracle, b"changed")

    def test_new_profile_is_opt_in_and_short_run_fails_before_launch(self) -> None:
        from unittest.mock import patch
        self.assertEqual(DEFAULT_PROFILES, ("idle", "1repo", "15overview", "15active", "soak"))
        with tempfile.TemporaryDirectory() as root, patch("bench_native_memory.NativeSession") as session:
            result = run_profile("1repo-diff", "/bin/true", root, root, root, 29.9, 0.05, 100)
        session.assert_not_called()
        self.assertEqual(result["status"], "FAILED")
        self.assertIn("at least 30", result["error"])

    def test_ref_picker_types_short_label_and_ignores_retired_bounds(self) -> None:
        from unittest.mock import MagicMock
        session = MagicMock()
        lines = ["[APP:CTRL_BOUNDS: id=btn-locale x=1000 y=10 w=40 h=20]",
                 "[APP:CTRL_BOUNDS: id=btn-ref-selector x=10 y=10 w=100 h=20]",
                 "[APP:CTRL_BOUNDS: id=selector-input x=10 y=40 w=100 h=20]",
                 f"[APP:CTRL_BOUNDS: id=pick-ref:{MATCHED_REF} x=10 y=80 w=100 h=20]",
                 f"[APP:CTRL_GONE: id=pick-ref:{MATCHED_REF}]"]
        session.texts.side_effect = lambda: lines
        session.lines = lines
        def typed(*args: str) -> None:
            self.assertEqual(args, ("xdotool", "type", "--clearmodifiers", "feat/divergent"))
            lines.extend([f"[APP:CTRL_BOUNDS: id=pick-ref:{MATCHED_REF} x=10 y=100 w=100 h=20]",
                          "[APP:CTRL_BOUNDS: id=commit-row:aaaaaaa x=10 y=600 w=600 h=20]",
                          "[APP:CTRL_BOUNDS: id=commit-file:a.txt x=10 y=200 w=100 h=20]"])
        session.x.side_effect = typed
        select_native_matched(session, {**self.window, "wid": "1"}, self.oracle)
        session.set_clipboard.assert_called_once_with(MATCHED_SENTINEL)
        self.assertEqual(session.click.call_args_list[0].args[1], (1000, 10, 40, 20))
        self.assertIn((10, 100, 100, 20), [call.args[1] for call in session.click.call_args_list])
        self.assertNotIn((10, 80, 100, 20), [call.args[1] for call in session.click.call_args_list])


class TestReceiptAndCompareFlag(unittest.TestCase):
    def test_history_page_size_is_not_a_cli_flag(self) -> None:
        err = io.StringIO()
        with redirect_stderr(err):
            with self.assertRaises(SystemExit) as cm:
                native_main([
                    "--bin", "/bin/true",
                    "--label", "pilot",
                    "--workspace", "/tmp",
                    "--out-dir", os.path.join(tempfile.gettempdir(), f"snip-no-page-{os.getpid()}"),
                    "--history-page-size", "25",
                ])
        self.assertEqual(cm.exception.code, 2)
        self.assertIn("unrecognized arguments", err.getvalue())
        self.assertIn("--history-page-size", err.getvalue())

    def test_compare_baseline_is_unsupported(self) -> None:
        err = io.StringIO()
        out = os.path.join(tempfile.gettempdir(), f"snip-d4-should-not-exist-{os.getpid()}")
        with redirect_stderr(err):
            code = native_main([
                "--bin", "/bin/true",
                "--label", "pilot",
                "--workspace", "/tmp",
                "--out-dir", out,
                "--compare-baseline", "/tmp/baseline.json",
            ])
        self.assertEqual(code, 2)
        self.assertIn("UNSUPPORTED", err.getvalue())
        self.assertIn("D4", err.getvalue())
        self.assertFalse(os.path.exists(out))

    def test_receipt_origin_and_hash_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_path = os.path.join(tmp, "app")
            payload = b"binary-bytes"
            with open(bin_path, "wb") as f:
                f.write(payload)
            os.chmod(bin_path, 0o755)
            actual = hashlib.sha256(payload).hexdigest()
            user = os.path.join(tmp, "user.json")
            with open(user, "w", encoding="utf-8") as f:
                json.dump({"sha256": actual, "command": "cargo build --release"}, f)
            got = load_build_receipt(bin_path, user)
            self.assertEqual(got["origin"], "user-supplied")
            self.assertTrue(got["sha256MatchesBinary"])
            self.assertEqual(got["document"]["command"], "cargo build --release")

            with open(user, "w", encoding="utf-8") as f:
                json.dump({"command": "cargo build --release"}, f)
            unchecked = load_build_receipt(bin_path, user)
            self.assertEqual(unchecked["origin"], "user-supplied")
            self.assertIsNone(unchecked["sha256MatchesBinary"])

            side = bin_path + ".receipt.json"
            with open(side, "w", encoding="utf-8") as f:
                json.dump({"sha256": actual}, f)
            sidecar = load_build_receipt(bin_path, None)
            self.assertEqual(sidecar["origin"], "sidecar")

            with open(user, "w", encoding="utf-8") as f:
                json.dump({"sha256": "ab" * 32}, f)
            with self.assertRaises(ValueError) as cm:
                load_build_receipt(bin_path, user)
            self.assertIn("does not match", str(cm.exception))

            out = os.path.join(tmp, "out")
            err = io.StringIO()
            with redirect_stderr(err):
                code = native_main([
                    "--bin", bin_path,
                    "--label", "pilot",
                    "--workspace", tmp,
                    "--out-dir", out,
                    "--build-receipt", user,
                    "--profile", "idle",
                ])
            self.assertEqual(code, 1)
            self.assertIn("does not match", err.getvalue())
            self.assertFalse(os.path.exists(out))


class TestProductionLauncher(unittest.TestCase):
    def test_shared_launcher_observes_exec_transition(self) -> None:
        require_or_skip(os.path.isdir("/proc"), "needs Linux /proc")
        sh = shutil.which("sh")
        require_or_skip(bool(sh), "sh is missing")
        sh = os.path.realpath(sh)
        self.assertIn("write_preexec_launcher", inspect.getsource(NativeSession._open))
        with tempfile.TemporaryDirectory() as tmp:
            launcher = os.path.join(tmp, "launcher.py")
            write_preexec_launcher(launcher)
            ident = os.path.join(tmp, "ident.json")
            gate = os.path.join(tmp, "gate")
            ready = os.path.join(tmp, "ready")
            script = f"sleep 0.08; echo '[READY:PROD]' > {shlex.quote(ready)}; sleep 1"
            proc = subprocess.Popen(
                [sys.executable, "-B", launcher, ident, gate, sh, "-c", script],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )
            try:
                data = None
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    if os.path.exists(ident):
                        try:
                            with open(ident, encoding="utf-8") as f:
                                data = json.load(f)
                            break
                        except json.JSONDecodeError:
                            pass
                    if proc.poll() is not None:
                        self.fail(f"launcher exited {proc.returncode} before writing identity")
                    time.sleep(0.01)
                self.assertIsNotNone(data)
                result = measure_single_profile(
                    attach_pid=data["pid"],
                    attach_starttime=data["starttime"],
                    expected_exe=sh,
                    sampler_ready_file=gate,
                    ready_file=ready,
                    ready_marker="[READY:PROD]",
                    profile_label="production launcher",
                    steady_seconds=0.25,
                    sample_interval=0.02,
                    readiness_timeout=4.0,
                )
            finally:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait(timeout=2)
            self.assertEqual(result["launchClaim"], "verified-pre-exec-gate")
            self.assertTrue(result["samplerGate"]["preExec"])
            self.assertNotEqual(result["samplerGate"]["exeAtPublish"], sh)
            self.assertIsNotNone(result["timestamps"]["execDiscoveredMonotonic"])
            self.assertGreater(result["sampleInterval"]["launchSampleCount"], 0)
            py = os.path.realpath(sys.executable)
            for sample in result["samples"]:
                if sample["phase"] == "launcher-setup":
                    self.assertEqual(sample["exe"], py)
                elif sample["phase"] in ("launch", "steady"):
                    self.assertEqual(sample["exe"], sh)
            setup = [s for s in result["samples"] if s["phase"] == "launcher-setup"]
            if setup:
                self.assertLess(
                    result["overallPeak"]["sampledPeakRssMib"],
                    min(s["totalRssMib"] for s in setup),
                )



class TestNativeSessionWaitApp(unittest.TestCase):
    def test_wait_app_fails_if_dbus_session_exits_early(self) -> None:
        from unittest.mock import MagicMock
        with tempfile.TemporaryDirectory() as tmp_dir:
            session = MagicMock()
            session.proc = MagicMock()
            session.proc.pid = 1234
            session.proc.poll.return_value = 1
            session.proc.returncode = 1
            session.bin_path = "/nonexistent/bin"
            session.ident_file = os.path.join(tmp_dir, "launcher_ident.json")
            with self.assertRaises(NativeBenchError) as cm:
                NativeSession.wait_app(session, on_app=lambda _: None, timeout=0.1)
            self.assertIn("exited with 1 before the app appeared", str(cm.exception))


class TestScrollHintScope(unittest.TestCase):
    VIEWPORT = "[APP:CTRL_BOUNDS: id=left-list x=36 y=66 w=280 h=398]"
    EAST = "[APP:REPO_SELECTING: 5 (east/docs) root=/fixture/machine-a/east/docs]"
    WEST = "[APP:REPO_SELECTING: 14 (west/docs) root=/fixture/machine-a/west/docs]"

    def first_wheel(self, lines: list[str], control: str) -> str:
        class WheelObserved(Exception):
            pass

        class Recorded:
            def __init__(self) -> None:
                self.lines = lines
                self.command: tuple[str, ...] = ()

            def texts(self, start: int = 0) -> list[str]:
                return self.lines[start:]

            def wait_line(self, pred: Any, start: int = 0, timeout: float = 10) -> tuple[int, float, str]:
                for index, line in enumerate(self.lines[start:], start):
                    if pred(line):
                        return index, 0.0, line
                raise NativeBenchError("recorded stream has no matching event")

            def focus(self, wid: str) -> None:
                pass

            def x(self, *args: str, timeout: float = 20) -> str:
                self.command = args
                raise WheelObserved

        session = Recorded()
        with self.assertRaises(WheelObserved):
            scroll_into_view(session, {"wid": "1", "x": 102, "y": 90}, control)
        self.assertEqual(session.command[:2], ("xdotool", "mousemove"))
        self.assertEqual(session.command[-2], "click")
        return session.command[-1]

    def test_repo_selection_retires_prior_same_path_hint(self) -> None:
        # Actual failed collision trace: east/docs src was y=204, then west/docs
        # loads below 15 repo rows. Its src is not yet rendered and needs DOWN.
        for selection in (self.WEST, self.EAST):
            with self.subTest(selection=selection):
                self.assertEqual(self.first_wheel([
                    self.VIEWPORT, self.EAST,
                    "[APP:CTRL_BOUNDS: id=tree-row:src x=36 y=204 w=280 h=24]",
                    selection,
                    "[APP:CTRL_GONE: id=tree-row:src]",
                    "[APP:TREE_PAGE: rel= kind=Expand children=9 has_more=false selected=0]",
                    "[APP:CTRL_BOUNDS: id=tree-row:assets x=36 y=426 w=280 h=24]",
                ], "tree-row:src"), "5")

    def test_repo_selection_keeps_workspace_repo_row_upward_hint(self) -> None:
        # Captured resource warmup: repo01 leaves above the viewport, then
        # repo12..15 load. Its workspace identity survives those selections.
        self.assertEqual(self.first_wheel([
            self.VIEWPORT,
            "[APP:CTRL_BOUNDS: id=repo-row:repo-01-core x=36 y=66 w=280 h=24]",
            "[APP:CTRL_GONE: id=repo-row:repo-01-core]",
            "[APP:REPO_SELECTING: 11 (repo-12-cli) root=/fixture/repo-12-cli]",
            "[APP:REPO_SELECTING: 14 (repo-15-admin) root=/fixture/repo-15-admin]",
            "[APP:CTRL_BOUNDS: id=repo-row:repo-15-admin x=36 y=368 w=280 h=24]",
        ], "repo-row:repo-01-core"), "4")

    def test_same_view_retains_gone_row_upward_hint(self) -> None:
        for control in ("tree-row:src", "rev-row:src", "rev-row:src/app.txt"):
            with self.subTest(control=control):
                self.assertEqual(self.first_wheel([
                    self.VIEWPORT, self.WEST, "[APP:REV_TREE: bbbbbbb]",
                    f"[APP:CTRL_BOUNDS: id={control} x=36 y=80 w=280 h=24]",
                    f"[APP:CTRL_GONE: id={control}]",
                ], control), "4")

    def test_revision_switch_retires_prior_path_hint(self) -> None:
        self.assertEqual(self.first_wheel([
            self.VIEWPORT, self.WEST, "[APP:REV_TREE: aaaaaaa]",
            "[APP:CTRL_BOUNDS: id=rev-row:src x=36 y=80 w=280 h=24]",
            "[APP:REV_TREE: off]", "[APP:REV_TREE: bbbbbbb]",
            "[APP:CTRL_GONE: id=rev-row:src]",
        ], "rev-row:src"), "5")


class TestScrollSettlesAfterReflow(unittest.TestCase):
    def test_uses_bounds_after_the_row_moves(self) -> None:
        class Moving:
            def __init__(self) -> None:
                self.n = 0
                self.lines: list[str] = []

            def texts(self, start: int = 0) -> list[str]:
                self.n += 1
                y = 259 if self.n < 4 else 211
                return [
                    "[APP:CTRL_BOUNDS: id=left-list x=0 y=40 w=300 h=400]",
                    f"[APP:CTRL_BOUNDS: id=repo-row:repo-04-auth x=10 y={y} w=200 h=24]",
                ]

            def wait_line(self, pred: Any, start: int = 0, timeout: float = 10) -> tuple[int, float, str]:
                return 0, 0.0, "id=left-list x=0"

            def focus(self, wid: str) -> None:
                return None

            def x(self, *args: str, timeout: float = 20) -> str:
                return ""

        box = scroll_into_view(
            Moving(),
            {"wid": "1", "x": 0, "y": 0, "width": 800, "height": 600},
            "repo-row:repo-04-auth",
            settle_seconds=0.12,
        )
        self.assertEqual(box, (10, 211, 200, 24))


    def test_scroll_into_view_resends_a_dropped_wheel(self) -> None:
        class Dropped:
            def __init__(self) -> None:
                self.lines: list[str] = []
                self.wheels = 0

            def texts(self, start: int = 0) -> list[str]:
                rows = ["[APP:CTRL_BOUNDS: id=left-list x=0 y=40 w=300 h=400]"]
                if self.wheels >= 2:
                    rows.append("[APP:CTRL_BOUNDS: id=tree-row:src x=10 y=200 w=200 h=24]")
                return rows

            def wait_line(self, pred: Any, start: int = 0, timeout: float = 10) -> tuple[int, float, str]:
                # The pre-scroll paint wait (0.5s) and the first wheel get no reaction.
                if timeout < 1 or self.wheels == 1:
                    raise NativeBenchError("timed out waiting for app log line")
                return 0, 0.0, "id=left-list x=0"

            def focus(self, wid: str) -> None:
                return None

            def x(self, *args: str, timeout: float = 20) -> str:
                self.wheels += 1
                return ""

        session = Dropped()
        box = scroll_into_view(session, {"wid": "1", "x": 0, "y": 0, "width": 800, "height": 600}, "tree-row:src", settle_seconds=0.05)
        self.assertEqual(box, (10, 200, 200, 24))
        self.assertEqual(session.wheels, 2)


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def _blank_session() -> NativeSession:
    session = NativeSession.__new__(NativeSession)
    session.proc = None
    session.xvfb = None
    session.owned = []
    session._clip_proc = None
    session._pipe_fds = []
    session.reader = None
    session.log = None
    session.xvfb_log = None
    session.iso_root = None
    session.app = None
    session.lines = []
    session.env = os.environ.copy()
    return session


class TestConstructorCleansPartialStart(unittest.TestCase):
    """A failed NativeSession must not leave the Xvfb it already started."""

    def _tracked(self) -> tuple[list[int], list[str], Any, Any]:
        pids: list[int] = []
        dirs: list[str] = []
        real_popen = subprocess.Popen
        real_mkdtemp = tempfile.mkdtemp

        def popen(args: list[str], *pos: Any, **kwargs: Any) -> subprocess.Popen:
            proc = real_popen(args, *pos, **kwargs)
            if args and args[0] == "Xvfb":
                pids.append(proc.pid)
            return proc

        def mkdtemp(*pos: Any, **kwargs: Any) -> str:
            path = real_mkdtemp(*pos, **kwargs)
            dirs.append(path)
            return path

        return pids, dirs, popen, mkdtemp

    def _assert_reaped(self, pids: list[int], dirs: list[str]) -> None:
        self.assertTrue(pids)
        for pid in pids:
            self.assertFalse(_pid_alive(pid), f"Xvfb {pid} still alive")
        for path in dirs:
            self.assertFalse(os.path.isdir(path), path)

    def test_display_not_ready_reaps_xvfb(self) -> None:
        import bench_native_memory as driver
        from unittest.mock import patch

        pids, dirs, popen, mkdtemp = self._tracked()
        with tempfile.TemporaryDirectory() as run_dir:
            with patch.object(driver.subprocess, "Popen", popen), \
                    patch.object(driver.tempfile, "mkdtemp", mkdtemp), \
                    patch.object(driver.select, "select", return_value=([], [], [])):
                with self.assertRaises(NativeBenchError) as caught:
                    NativeSession("/bin/true", run_dir, "idle", run_dir, False)
        self.assertIn("display", str(caught.exception))
        self._assert_reaped(pids, dirs)

    def test_launcher_oserror_reaps_xvfb_and_stop_is_idempotent(self) -> None:
        import bench_native_memory as driver
        from unittest.mock import patch

        pids, dirs, real_popen, mkdtemp = self._tracked()
        instances: list[NativeSession] = []
        real_init = NativeSession.__init__

        def popen(args: list[str], *pos: Any, **kwargs: Any) -> subprocess.Popen:
            if args and args[0] != "Xvfb":
                raise OSError("injected launcher failure")
            return real_popen(args, *pos, **kwargs)

        def init(self: NativeSession, *args: Any, **kwargs: Any) -> None:
            instances.append(self)
            real_init(self, *args, **kwargs)

        with tempfile.TemporaryDirectory() as run_dir:
            with patch.object(driver.subprocess, "Popen", popen), \
                    patch.object(driver.tempfile, "mkdtemp", mkdtemp), \
                    patch.object(NativeSession, "__init__", init):
                with self.assertRaises(OSError) as caught:
                    NativeSession("/bin/true", run_dir, "idle", run_dir, False)
        self.assertIn("injected launcher failure", str(caught.exception))
        self._assert_reaped(pids, dirs)
        self.assertEqual(instances[0].stop(), [])
        self._assert_reaped(pids, dirs)


class TestLogReaderCleanup(unittest.TestCase):
    def test_stop_closes_stdout_after_the_reader_joins(self) -> None:
        session = _blank_session()
        # Own process group: stop() signals that group, and must not signal the test.
        session.proc = subprocess.Popen(
            ["sleep", "30"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True,
        )
        stdout = session.proc.stdout
        assert stdout is not None
        with tempfile.TemporaryDirectory() as tmp:
            session.log = open(os.path.join(tmp, "app.log"), "w")
            session.reader = threading.Thread(target=session._read, daemon=True)
            session.reader.start()
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always", ResourceWarning)
                problems = session.stop(reader_join_timeout=2)
                again = session.stop(reader_join_timeout=0.2)
                del session
                gc.collect()
        self.assertEqual(problems, [])
        self.assertEqual(again, [])
        self.assertTrue(stdout.closed)
        self.assertFalse(any(item.category is ResourceWarning for item in caught))

    def test_stop_reports_when_the_reader_does_not_finish(self) -> None:
        session = _blank_session()
        release = threading.Event()
        with tempfile.TemporaryDirectory() as tmp:
            session.log = open(os.path.join(tmp, "app.log"), "w")
            session.reader = threading.Thread(target=release.wait, kwargs={"timeout": 30}, daemon=True)
            session.reader.start()
            problems = session.stop(reader_join_timeout=0.2)
            self.assertTrue(problems)
            self.assertFalse(session.log.closed)
            self.assertTrue(session.reader.is_alive())
            release.set()
            self.assertEqual(session.stop(reader_join_timeout=2), [])
            self.assertFalse(session.reader.is_alive())
            self.assertTrue(session.log.closed)


class TestForegroundClipboardOwner(unittest.TestCase):
    def test_quiet_xclip_is_reaped_while_xvfb_stays_up(self) -> None:
        log_dir = tempfile.mkdtemp(prefix="snip-xclip-owner-")
        xvfb_log = open(os.path.join(log_dir, "xvfb.log"), "wb")
        read_fd, write_fd = os.pipe()
        xvfb = subprocess.Popen(
            ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "640x480x24", "-nolisten", "tcp", "-noreset"],
            pass_fds=(write_fd,), stdout=xvfb_log, stderr=xvfb_log, start_new_session=True,
        )
        os.close(write_fd)
        try:
            ready, _, _ = select.select([read_fd], [], [], 15)
            display = os.read(read_fd, 64).decode().strip() if ready else ""
            self.assertTrue(display.isdigit(), display)
            session = _blank_session()
            session.env["DISPLAY"] = f":{display}"
            session.env.pop("WAYLAND_DISPLAY", None)
            payload = b"SNIP-DRIVER-CLIP-OWNER\n"
            session.set_clipboard(payload)
            proc = session._clip_proc
            assert proc is not None
            self.assertIsNone(proc.poll())
            with open(f"/proc/{proc.pid}/cmdline", "rb") as handle:
                cmdline = handle.read().replace(b"\x00", b" ")
            self.assertIn(b"-quiet", cmdline)
            self.assertNotIn(b"-silent", cmdline)
            with open(f"/proc/{proc.pid}/status", encoding="utf-8") as handle:
                status = handle.read()
            ppid = next(line for line in status.splitlines() if line.startswith("PPid:"))
            self.assertNotIn("1", ppid.split()[-1:])
            self.assertEqual(ppid.split()[-1], str(os.getpid()))
            self.assertEqual(session.read_clipboard(timeout=2), payload)
            self.assertTrue(_pid_alive(xvfb.pid))
            problems = session.stop(reader_join_timeout=0.2)
            self.assertEqual(problems, [])
            self.assertFalse(_pid_alive(proc.pid))
            self.assertTrue(_pid_alive(xvfb.pid))
        finally:
            os.close(read_fd)
            try:
                os.killpg(os.getpgid(xvfb.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            xvfb.wait(timeout=2)
            xvfb_log.close()
            shutil.rmtree(log_dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
