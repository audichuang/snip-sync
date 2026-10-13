"""Protect the matched benchmark's transport and measurement assumptions."""
from __future__ import annotations

import hashlib
import io
import itertools
import json
from pathlib import Path
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import bench_remote_paste as bench


class Fragmented(io.BytesIO):
    def read(self, size=-1):
        return super().read(min(size, 2) if size >= 0 else 2)


class Frames(unittest.TestCase):
    def test_frame_boundaries_survive_short_reads(self):
        bodies = [b'{"op":"hello","version":1}', b'{"op":"chunk","data":"abc"}']
        stream = Fragmented(b"".join(struct.pack(">I", len(b)) + b for b in bodies))
        self.assertEqual([bench.read_frame(stream), bench.read_frame(stream)], bodies)
        self.assertIsNone(bench.read_frame(stream))

    def test_truncated_header_and_body_are_not_clean_eof(self):
        for raw in [b"\x00", struct.pack(">I", 5), struct.pack(">I", 5) + b"ab"]:
            with self.subTest(raw=raw), self.assertRaises(EOFError):
                bench.read_frame(Fragmented(raw))

    def test_oversize_refused_before_reading_body(self):
        stream = io.BytesIO(struct.pack(">I", bench.FRAME_MAX + 1) + b"do not consume")
        with self.assertRaises(ValueError):
            bench.read_frame(stream)
        self.assertEqual(stream.tell(), 4)

    def test_meter_reads_kind_without_reencoding_utf8_frames(self):
        body = json.dumps({"reply": "hello", "version": 1, "max_version": 6,
                           "name": "工作機", "home": "/home/使用者"}, ensure_ascii=False).encode()
        stream = Fragmented(struct.pack(">I", len(body)) + body)
        observed = bench.read_frame(stream)
        self.assertEqual(observed, body)
        self.assertEqual(bench.frame_kind(observed, "down"), "hello")
        self.assertEqual(bench.frame_kind(b'{"op":"frame_chunk","data":"..."}', "up"), "frame_chunk")


class Corpus(unittest.TestCase):
    def test_all_full_scale_cases_are_bounded_and_identified(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "crates").mkdir()
            rust = root / "crates" / "sample.rs"
            rust.write_text("pub fn sample() {}\n" * 62000)
            for case in ["rust", "many", "large", "utf8"]:
                with self.subTest(case=case):
                    payload, metadata = bench.corpus(root, case, 1)
                    self.assertLessEqual(len(payload), 32 * 1024 * 1024)
                    self.assertGreater(len(payload), 0)
                    payload.decode("utf-8")
                    self.assertEqual(metadata["payload_bytes"], len(payload))
                    self.assertEqual(metadata["payload_sha256"], hashlib.sha256(payload).hexdigest())
                    self.assertGreater(metadata["entries"], 0)
                    if case == "rust":
                        self.assertEqual(metadata["source"][0]["sha256"], bench.sha(rust))
                    if case == "many":
                        self.assertEqual(metadata["entries"], 1200)
                    if case == "large":
                        self.assertGreater(len(payload), 31 * 1024 * 1024)
                    if case == "utf8":
                        self.assertGreater(metadata["zlib_ratio_description_only"], 0.6)

    def test_low_compression_utf8_uses_a_reproducible_seed(self):
        first, metadata = bench.corpus(Path("unused"), "utf8", 0.01)
        second, repeated = bench.corpus(Path("unused"), "utf8", 0.01)
        self.assertEqual((first, metadata), (second, repeated))

    def test_rust_fixture_headers_do_not_create_extra_payload_entries(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "crates").mkdir()
            (root / "crates" / "a.rs").write_text('// FILE: unintended.rs\nfixture\n')
            (root / "crates" / "b.rs").write_text('fn real_source() {}\n')
            payload, metadata = bench.corpus(root, "rust", 0.01)
            self.assertNotIn(b"unintended.rs", payload)
            self.assertIn(b"fn real_source()", payload)
            self.assertEqual(metadata["entries"], 1)


class Metrics(unittest.TestCase):
    def test_darwin_cpu_converts_mach_ticks_and_excludes_thread_totals(self):
        def task_info(pid, flavor, argument, output, size):
            self.assertEqual((pid, flavor, argument), (321, 4, 0))
            output._obj.user = 18_000_000
            output._obj.system = 6_000_000
            output._obj.threads_user = 9_000_000_000
            output._obj.threads_system = 9_000_000_000
            output._obj.resident = 123456
            return size

        api = SimpleNamespace(proc_pidinfo=task_info)
        with mock.patch.object(bench.sys, "platform", "darwin"), \
             mock.patch.object(bench, "darwin_process_api", return_value=(api, 125 / 3 / 1e9)):
            measured = bench.process_sample(321)
        self.assertAlmostEqual(measured["cpu_seconds"], 1)
        self.assertEqual(measured["rss_bytes"], 123456)

    def test_darwin_timebase_comes_from_the_machine(self):
        def timebase(output):
            output._obj.numer = 125
            output._obj.denom = 3
            return 0

        def proc_pidinfo(*_args):
            return 0

        library = SimpleNamespace(proc_pidinfo=proc_pidinfo)
        bench.darwin_process_api.cache_clear()
        try:
            with mock.patch.object(bench.ctypes, "CDLL", side_effect=[
                    SimpleNamespace(mach_timebase_info=timebase), library]):
                result, seconds_per_tick = bench.darwin_process_api()
            self.assertIs(result, library)
            self.assertAlmostEqual(seconds_per_tick * 24_000_000, 1)
        finally:
            bench.darwin_process_api.cache_clear()

    def test_linux_cpu_is_this_process_not_its_children(self):
        # /proc stat tail begins at field 3; include ')' inside comm so the
        # parser must use the final delimiter. Child CPU is deliberately huge.
        fields = ["0"] * 22
        fields[0] = "S"
        fields[11:15] = ["30", "20", "100000", "200000"]
        fields[21] = "9"
        raw = "321 (worker ) name) " + " ".join(fields)
        with mock.patch.object(bench.sys, "platform", "linux"), \
             mock.patch.object(Path, "read_text", return_value=raw) as read, \
             mock.patch.object(bench.os, "sysconf", create=True,
                               side_effect=lambda name: 100 if name == "SC_CLK_TCK" else 4096):
            self.assertEqual(bench.process_sample(321), {"cpu_seconds": 0.5, "rss_bytes": 9 * 4096})
            read.assert_called_once_with()

    def test_missing_pid_does_not_become_zero_cpu(self):
        with mock.patch.object(bench.sys, "platform", "linux"), \
             mock.patch.object(Path, "read_text", side_effect=FileNotFoundError):
            self.assertIsNone(bench.process_sample(321))

    def test_summary_keeps_versions_connections_and_cases_separate(self):
        def sample(case, version, connection, elapsed):
            return {"case": case, "version": version, "connection": connection,
                    "operation": {"elapsed_seconds": elapsed},
                    "master": {"cpu_seconds": elapsed / 2, "sampled_peak_rss_bytes": 100},
                    "workers": [{"worker": {"cpu_seconds": 0.1, "sampled_peak_rss_bytes": 200}},
                                {"worker": {"cpu_seconds": 0.3, "sampled_peak_rss_bytes": 300}}],
                    "wire": {"up": 11, "down": 22}}
        data = [sample("rust", 6, "cold", 2), sample("rust", 6, "cold", 4),
                sample("rust", 6, "warm", 100), sample("rust", 7, "cold", 200),
                sample("many", 6, "cold", 300)]
        report = bench.summary(data)
        self.assertEqual(len(report), 4)
        cold = report["rust/v6/cold"]
        self.assertEqual(cold["runs"], 2)
        self.assertEqual(cold["elapsed_median_seconds"], 3)
        self.assertEqual(cold["master_cpu_median_seconds"], 1.5)
        self.assertAlmostEqual(cold["worker_cpu_median_seconds"], 0.4)
        self.assertEqual(cold["up_framed_bytes"], [11, 11])
        self.assertEqual(cold["master_sampled_peak_rss_median_bytes"], 100)
        self.assertEqual(cold["worker_max_process_sampled_peak_rss_median_bytes"], 300)
        self.assertEqual(cold["worker_per_process_peaks_bytes"], [[200, 300], [200, 300]])


class Cleanup(unittest.TestCase):
    def test_pid_reuse_is_not_permission_to_signal_the_new_process(self):
        old = {"pid": 123, "start": "before", "zombie": False}
        new = {"pid": 123, "start": "after", "zombie": False}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "process-123.json").write_text(json.dumps({"bridge": old, "child": old}))
            with mock.patch.object(bench, "process_identity", return_value=new), \
                 mock.patch.object(bench.os, "kill") as kill:
                self.assertTrue(bench.finish_metrics(directory)["completed"])
                kill.assert_not_called()

    def test_unknown_identity_is_neither_killed_nor_reported_as_exited(self):
        unknown = {"pid": 123, "start": None, "zombie": False}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "process-123.json").write_text(json.dumps({"bridge": unknown, "child": unknown}))
            with mock.patch.object(bench, "process_identity", return_value=unknown), \
                 mock.patch.object(bench.time, "monotonic", side_effect=itertools.count(0, 10)), \
                 mock.patch.object(bench.os, "kill") as kill:
                self.assertFalse(bench.finish_metrics(directory)["completed"])
                kill.assert_not_called()

    def test_missing_process_registry_is_not_cleanup_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            self.assertFalse(bench.finish_metrics(Path(temp))["completed"])

    def test_failed_dataset_cleans_only_this_remote_round_and_records_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "test-binary"
            binary.write_bytes(b"test fixture, never executed")
            args = SimpleNamespace(ssh="isolated-test", remote_snip="/tmp/test-snip",
                                   worker_source_sha="a" * 40, worker=binary, driver=binary,
                                   out=root / "results", repo=root, cases="invalid-case",
                                   rounds=10, scale=1)
            allocated = "/tmp/snip-paste-bench-Ab12Cd34"
            calls = []

            def remote(host, argv, data=None):
                self.assertEqual(host, "isolated-test")
                calls.append(argv)
                if argv[0] == "mktemp":
                    return (allocated + "\n").encode()
                if "binary-hash" in argv:
                    return json.dumps("b" * 64).encode()
                return b""

            with mock.patch.object(bench, "remote_call", side_effect=remote), \
                 mock.patch.object(bench.subprocess, "check_output", side_effect=["a" * 40, ""]):
                with self.assertRaisesRegex(ValueError, "invalid-case"):
                    bench.run(args)
            self.assertEqual(calls[-1], ["rm", "-rf", "--", allocated])
            report = json.loads((args.out / "results.json").read_text())
            self.assertEqual(report["cleanup"], {"remote_root": allocated,
                                                "processes_confirmed_ended": True, "completed": True})
            self.assertIn("invalid-case", report["failure"])
            self.assertEqual(report["samples"], [])


if __name__ == "__main__":
    unittest.main()
