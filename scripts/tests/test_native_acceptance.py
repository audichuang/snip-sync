"""Entrypoint plumbing only: fake artifacts never stand in for GUI acceptance."""

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_native_acceptance as acceptance
from collaboration_fixture import validate_target


class AcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "native"
        self.binary.write_bytes(b"synthetic test artifact, never executed")
        self.binary.chmod(0o755)
        self.source = {"head": "abc", "tree": "def", "status": " M file\0", "files": {}}
        self.data = {"producer": acceptance.PRODUCER, "source": self.source,
                     "sourceSha": "abc", "binary": str(self.binary),
                     "sha256": acceptance.sha256_file(str(self.binary)),
                     "buildCommand": acceptance.BUILD, "buildExitCode": 0, "buildProfile": "release"}
        self.receipt = self.root / "receipt.json"
        acceptance.write_json(self.receipt, self.data)

    def test_frozen_receipt_rejects_source_binary_and_build_claim_changes(self):
        with mock.patch.object(acceptance, "source_snapshot", return_value=self.source):
            self.assertEqual(acceptance.verify_build(self.receipt)["sourceSha"], "abc")
            self.binary.write_bytes(b"different executable")
            with self.assertRaisesRegex(ValueError, "executable changed"):
                acceptance.verify_build(self.receipt)
        self.binary.write_bytes(b"synthetic test artifact, never executed")
        for key, value in (("head", "changed"), ("tree", "changed"), ("status", ""),
                           ("files", {"source.rs": {"sha256": "different"}})):
            with self.subTest(key=key):
                changed = dict(self.source, **{key: value})
                with mock.patch.object(acceptance, "source_snapshot", return_value=changed):
                    with self.assertRaisesRegex(ValueError, "checkout"):
                        acceptance.verify_build(self.receipt)
        self.data["buildCommand"] = ["echo", "not a build"]
        acceptance.write_json(self.receipt, self.data)
        with self.assertRaisesRegex(ValueError, "successful release build"):
            acceptance.verify_build(self.receipt)

    def test_changed_checkout_names_the_files(self):
        after = dict(self.source, files={"native-acceptance/lifecycle.log": {"sha256": "x"}})
        with mock.patch.object(acceptance, "source_snapshot", return_value=after):
            with self.assertRaisesRegex(ValueError, "added native-acceptance/lifecycle.log"):
                acceptance.verify_build(self.receipt)

    def test_snapshot_hashes_actual_dirty_and_untracked_files(self):
        (self.root / "source.rs").write_text("before")
        (self.root / "new.rs").write_text("new input")

        def git(*args):
            if args[0] == "ls-files":
                return b"source.rs\0new.rs\0deleted.rs\0"
            if args[0] == "status":
                return b" M source.rs\0?? new.rs\0 D deleted.rs\0"
            return b"same commit and tree\n"

        with mock.patch.object(acceptance, "ROOT", self.root), mock.patch.object(acceptance, "git", side_effect=git):
            before = acceptance.source_snapshot()
            (self.root / "source.rs").write_text("after")
            after = acceptance.source_snapshot()
        self.assertEqual(before["head"], after["head"])
        self.assertNotEqual(before["files"]["source.rs"], after["files"]["source.rs"])
        self.assertIn("new.rs", after["files"])
        self.assertTrue(after["files"]["deleted.rs"]["missing"])

    def test_build_freezes_observed_artifact_and_refuses_midbuild_mutation(self):
        def fake_build(command, output, name, commands, ok=(0,)):
            self.assertEqual(command, acceptance.BUILD)
            (output / "build.log").write_text(json.dumps({"reason": "compiler-artifact",
                "target": {"name": "snip-desktop-native"}, "executable": str(self.binary)}) + "\n")

        output = self.root / "built"
        output.mkdir()
        with mock.patch.object(acceptance, "run", side_effect=fake_build), \
                mock.patch.object(acceptance, "source_snapshot", return_value=self.source):
            receipt = acceptance.build(output, [])
            data = acceptance.verify_build(receipt)
        self.assertEqual(Path(data["binary"]).read_bytes(), self.binary.read_bytes())
        self.assertNotEqual(data["binary"], str(self.binary))
        changed = dict(self.source, status="changed while building")
        failed = self.root / "mutated"
        failed.mkdir()
        with mock.patch.object(acceptance, "run", side_effect=fake_build), \
                mock.patch.object(acceptance, "source_snapshot", side_effect=[self.source, changed]):
            with self.assertRaisesRegex(ValueError, "source changed"):
                acceptance.build(failed, [])
        self.assertFalse((failed / "build-receipt.json").exists())
        self.assertNotEqual(*json.loads((failed / "build-inputs.json").read_text()).values())

    def test_fresh_outputs_never_delete_previous_evidence(self):
        evidence = self.root / "existing"
        evidence.mkdir()
        marker = evidence / "result.json"
        marker.write_text("preserve")
        with self.assertRaisesRegex(ValueError, "must not exist"):
            acceptance.fresh_output(evidence)
        self.assertEqual(marker.read_text(), "preserve")

    def test_output_contract_rejects_checkout_before_build_and_matches_fixture_guard(self):
        inside = acceptance.ROOT / "target" / "native-acceptance" / self.root.name
        self.assertFalse(inside.exists())
        def remove_test_output():
            if inside.exists():
                (inside / "acceptance.json").unlink(missing_ok=True)
                inside.rmdir()
        self.addCleanup(remove_test_output)
        with mock.patch.object(acceptance, "prerequisites"), \
                mock.patch.object(acceptance, "build", return_value=self.receipt) as build, \
                mock.patch.object(acceptance, "verify_build", return_value=self.data):
            self.assertNotEqual(acceptance.main(["--gate", "build", "--output", str(inside)]), 0)
        build.assert_not_called()
        self.assertFalse(inside.exists())
        output = acceptance.fresh_output(None)
        self.addCleanup(output.rmdir)
        self.assertFalse(output.is_relative_to(acceptance.ROOT))
        fixture, status = validate_target(str(output / "collaboration-fixture"))
        self.assertEqual(fixture.parent, output)
        self.assertEqual(status, "missing")

    def test_gate_commands_preserve_full_oracles_and_isolated_interpreter(self):
        commands = []

        driver_env = {}

        def collect(command, output, name, records, ok=(0,), extra_env=None):
            commands.append(command)
            if name in acceptance.DRIVER_TESTS:
                driver_env[name] = extra_env
                for shot in acceptance.SMOKE_SCREENSHOTS:
                    (Path(extra_env["SNIP_E2E_OUT"]) / shot).write_bytes(b"\x89PNG\r\n\x1a\n")
            if name == "collaboration-fixture":
                fixture = output / "collaboration-fixture"
                fixture.mkdir()
                (fixture / "manifest.json").write_text(json.dumps({"datasetHash": "canonical hash"}))
            elif name.startswith("workload-"):
                fixture = output / name
                fixture.mkdir()
                (fixture / "workload_manifest.json").write_text("manifest bytes")

        with mock.patch.object(acceptance, "run", side_effect=collect):
            for gate in (*acceptance.GATES, "resource-long"):
                acceptance.run_gate(gate, self.root, self.receipt, self.data, [])
        drivers = [c for c in commands if "snip-native-e2e" in c]
        self.assertEqual([c[c.index("--test") + 1] for c in drivers], ["smoke", "lifecycle"])
        # The drivers run the frozen receipt binary, never a fresh debug build.
        self.assertEqual({env["SNIP_NATIVE_BIN"] for env in driver_env.values()}, {str(self.binary)})
        self.assertTrue(all(command[:2] == [sys.executable, "-B"]
                            for command in commands if command not in drivers))
        ime = next(c for c in commands if "scripts/check_native_ime_startup.py" in c)
        self.assertIn("--output", ime)
        collaboration = next(c for c in commands if "scripts/check_native_collaboration.py" in c)
        self.assertEqual(collaboration[collaboration.index("--phase") + 1], "all")
        self.assertNotIn("--steps", collaboration)
        presets = [c[c.index("--preset") + 1] for c in commands if "--preset" in c]
        self.assertEqual(presets, ["medium", "standard"])
        leaks = [c for c in commands if "scripts/check_native_leaks.py" in c]
        self.assertEqual([c[c.index("--profile") + 1] for c in leaks], ["short", "long"])
        for command in leaks:
            self.assertNotIn("--measured-switches", command)
            self.assertNotIn("--warmup-switches", command)
            self.assertNotIn("--settle-seconds", command)
            self.assertNotIn("--absolute-release-budget", command)
            self.assertIn("--build-receipt", command)

    def test_environment_and_missing_requirements_fail_closed(self):
        with mock.patch.dict(os.environ, {"DISPLAY": ":1", "WAYLAND_DISPLAY": "host",
                                         "DBUS_SESSION_BUS_ADDRESS": "host", "GIT_DIR": "elsewhere",
                                         "SNIP_REQUIRE_ALL_TESTS": ""}):
            env = acceptance.environment()
            self.assertNotIn("DISPLAY", env)
            self.assertNotIn("WAYLAND_DISPLAY", env)
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", env)
            self.assertNotIn("GIT_DIR", env)
            self.assertEqual(env["SNIP_REQUIRE_ALL_TESTS"], "1")
        with mock.patch.object(acceptance.sys, "platform", "linux"), \
                mock.patch.object(acceptance.sys, "byteorder", "little"), \
                mock.patch.object(acceptance.shutil, "which", return_value=None):
            with self.assertRaisesRegex(ValueError, "required tools missing"):
                acceptance.prerequisites(("resource-short",), building=False)
        with mock.patch.object(acceptance.sys, "platform", "darwin"):
            with self.assertRaisesRegex(ValueError, "refusing to skip"):
                acceptance.prerequisites((), building=False)
        with mock.patch.object(acceptance.sys, "platform", "linux"), \
                mock.patch.object(acceptance.sys, "byteorder", "little"), \
                mock.patch.object(acceptance.shutil, "which", return_value="/available/tool"), \
                mock.patch.object(acceptance.importlib, "import_module", side_effect=ImportError("no Pillow")) as image:
            with self.assertRaisesRegex(ValueError, "Pillow is required"):
                acceptance.prerequisites(("ime",), building=False)
            image.assert_called_once_with("PIL.Image")
            image.reset_mock()
            acceptance.prerequisites(("collaboration", "resource-short", "resource-long"), building=False)
            image.assert_not_called()  # These drivers use ImageMagick, not Pillow.

    def test_all_builds_once_and_checks_source_after_failed_gate(self):
        output = self.root / "all"
        with mock.patch.object(acceptance, "prerequisites"), \
                mock.patch.object(acceptance, "build", return_value=self.receipt) as build, \
                mock.patch.object(acceptance, "verify_build", return_value=self.data) as verify, \
                mock.patch.object(acceptance, "run_gate") as gate:
            self.assertEqual(acceptance.main(["--output", str(output)]), 0)
        build.assert_called_once()
        self.assertEqual([call.args[0] for call in gate.call_args_list], list(acceptance.GATES))
        self.assertEqual(verify.call_count, 2 * len(acceptance.GATES) + 2)
        failed = self.root / "failed"
        with mock.patch.object(acceptance, "prerequisites"), \
                mock.patch.object(acceptance, "verify_build", return_value=self.data) as verify, \
                mock.patch.object(acceptance, "run_gate", side_effect=RuntimeError("long missing-coverage")):
            self.assertEqual(acceptance.main(["--gate", "resource-long", "--output", str(failed),
                                              "--build-receipt", str(self.receipt)]), 1)
        self.assertEqual(verify.call_count, 3)
        report = json.loads((failed / "acceptance.json").read_text())
        self.assertEqual(report["status"], "FAILED")
        self.assertFalse(report["fullD4Claimed"])

    def test_parallel_gates_each_run_once_and_keep_gate_order(self):
        def gate(name, output, receipt, data, records, shard):
            records.append({"command": [name]})

        output = self.root / "parallel"
        with mock.patch.object(acceptance, "prerequisites"), \
                mock.patch.object(acceptance, "verify_build", return_value=self.data), \
                mock.patch.object(acceptance, "run_gate", side_effect=gate):
            self.assertEqual(acceptance.main(["--output", str(output), "--jobs", "3",
                                              "--build-receipt", str(self.receipt)]), 0)
        report = json.loads((output / "acceptance.json").read_text())
        self.assertEqual([c["command"][0] for c in report["commands"]], list(acceptance.GATES))

    def test_real_child_exit_status_and_stderr_are_preserved_in_log(self):
        commands = []
        with self.assertRaisesRegex(RuntimeError, "exit 7"):
            acceptance.run([sys.executable, "-c", "import sys; print('failure', file=sys.stderr); sys.exit(7)"],
                           self.root, "failure", commands)
        self.assertEqual(commands[0]["exitCode"], 7)
        self.assertIn("failure", (self.root / "failure.log").read_text())

    def test_failed_source_binary_and_prerequisites_write_failed_summary(self):
        for reason in ("source", "binary", "prerequisite"):
            with self.subTest(reason=reason):
                self.binary.write_bytes(b"synthetic test artifact, never executed")
                source = self.source
                if reason == "source":
                    source = dict(self.source, head="different head")
                if reason == "binary":
                    self.binary.write_bytes(b"different binary")
                error = ValueError("missing prerequisite") if reason == "prerequisite" else None
                output = self.root / reason
                with mock.patch.object(acceptance, "prerequisites", side_effect=error), \
                        mock.patch.object(acceptance, "source_snapshot", return_value=source), \
                        mock.patch.object(acceptance, "run_gate") as gate:
                    code = acceptance.main(["--gate", "ime", "--output", str(output),
                                            "--build-receipt", str(self.receipt)])
                self.assertNotEqual(code, 0)
                gate.assert_not_called()
                report = json.loads((output / "acceptance.json").read_text())
                self.assertEqual(report["status"], "FAILED")
                self.assertTrue(report["error"])

    def shard(self, name, gates, steps=(), status="PASSED", sha=None, ids=("a", "b", "c")):
        """A shard output as a CI job leaves it; only what merge reads."""
        path = self.root / name
        path.mkdir()
        acceptance.write_json(path / "acceptance.json", {
            "status": status, "gates": list(gates), "commands": [{"command": [name]}],
            "binarySha256": sha or self.data["sha256"], "sourceSha": "abc"})
        if "collaboration" in gates:
            (path / "collaboration-fixture").mkdir()
            acceptance.write_json(path / "collaboration-fixture" / "manifest.json",
                                  {"steps": [{"id": step_id} for step_id in ids]})
            (path / "collaboration").mkdir()
            acceptance.write_json(path / "collaboration" / "report.json", {
                "binarySha256": "b" * 64, "datasetHash": "d" * 64,
                "steps": [{"id": step_id, "status": "passed"} for step_id in steps]})
        return path

    @staticmethod
    def full_check(report, required_ids=("a", "b", "c")):
        """Stands in for the driver's check: only the step set matters here."""
        ids = [step["id"] for step in report["steps"]]
        return [] if sorted(ids) == sorted(required_ids) else [f"steps {ids}"]

    def test_collaboration_shards_split_every_step_exactly_once(self):
        manifest = {"steps": [{"id": str(n)} for n in range(18)]}
        shards = [acceptance.shard_steps(manifest, (k, 2)) for k in (1, 2)]
        self.assertEqual(sorted(shards[0] + shards[1], key=int), [str(n) for n in range(18)])
        self.assertFalse(set(shards[0]) & set(shards[1]))
        for bad in ("0/2", "3/2", "x", "1/2/3"):
            with self.subTest(bad=bad), self.assertRaises(Exception):
                acceptance.parse_shard(bad)

    def test_merge_needs_every_gate_and_step_from_this_build(self):
        good = [self.shard("ime-res", ("ime", "resource-short", "smoke", "lifecycle")),
                self.shard("c1", ("collaboration",), steps=("a", "c")),
                self.shard("c2", ("collaboration",), steps=("b",))]
        output = self.root / "merged"
        with mock.patch.object(acceptance, "verify_build", return_value=self.data), \
                mock.patch.object(acceptance.collaboration, "functional_problems", side_effect=self.full_check):
            self.assertEqual(acceptance.main(["--gate", "merge", "--output", str(output),
                "--build-receipt", str(self.receipt), "--merge", *map(str, good)]), 0)
        report = json.loads((output / "acceptance.json").read_text())
        # release.yml accepts exactly this shape.
        self.assertEqual((report["status"], report["gate"]), ("PASSED", "all"))
        self.assertEqual(len(report["commands"]), 3)
        bad_sets = {
            "lost step": [good[0], good[1]],
            "lost gate": [good[1], good[2]],
            "failed shard": [good[0], good[1], self.shard("c2f", ("collaboration",), ("b",), "FAILED")],
            "other build": [good[0], good[1], self.shard("c2x", ("collaboration",), ("b",), sha="0" * 64)],
            "step twice": [*good, self.shard("c3", ("collaboration",), steps=("b",))],
        }
        for reason, shards in bad_sets.items():
            with self.subTest(reason=reason):
                output = self.root / f"merged-{reason}"
                with mock.patch.object(acceptance, "verify_build", return_value=self.data), \
                        mock.patch.object(acceptance.collaboration, "functional_problems",
                                          side_effect=self.full_check):
                    code = acceptance.main(["--gate", "merge", "--output", str(output),
                        "--build-receipt", str(self.receipt), "--merge", *map(str, shards)])
                self.assertEqual(code, 1)
                self.assertEqual(json.loads((output / "acceptance.json").read_text())["status"], "FAILED")


if __name__ == "__main__":
    unittest.main()
