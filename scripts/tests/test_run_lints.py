"""run_lints.sh runs every scripts/check_*.sh and fails when one does (#230)."""

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "run_lints.sh"
REPO = Path(__file__).resolve().parents[2]


def run_in(lints: dict[str, int]) -> subprocess.CompletedProcess:
    """A scripts/ folder holding run_lints.sh and one stub per entry, each
    exiting with the given code."""
    with tempfile.TemporaryDirectory() as tmp:
        scripts = Path(tmp) / "scripts"
        scripts.mkdir()
        shutil.copy(SCRIPT, scripts / "run_lints.sh")
        for name, code in lints.items():
            (scripts / name).write_text(f"#!/usr/bin/env bash\nexit {code}\n")
        return subprocess.run(
            ["bash", str(scripts / "run_lints.sh")],
            capture_output=True,
            text=True,
            timeout=60,
        )


class RunLints(unittest.TestCase):
    def test_all_passing_lints_pass(self):
        r = run_in({"check_a.sh": 0, "check_b.sh": 0})
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_one_failing_lint_fails_the_run_and_is_named(self):
        r = run_in({"check_a.sh": 0, "check_b.sh": 1, "check_c.sh": 0})
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("check_b.sh", r.stderr)
        self.assertNotIn("check_a.sh", r.stderr)

    def test_a_failure_does_not_stop_the_lints_after_it(self):
        r = run_in({"check_a.sh": 1, "check_b.sh": 1})
        self.assertIn("check_a.sh", r.stderr)
        self.assertIn("check_b.sh", r.stderr)

    def test_a_folder_with_no_lint_is_an_error(self):
        r = run_in({})
        self.assertNotEqual(r.returncode, 0)

    def test_it_runs_the_real_lints(self):
        # What a gate runs: every check_*.sh that is in the tree.
        names = sorted(p.name for p in (REPO / "scripts").glob("check_*.sh"))
        self.assertIn("check_generation_types.sh", names)
        r = subprocess.run(
            ["bash", str(SCRIPT)],
            capture_output=True,
            text=True,
            timeout=120,
            cwd=REPO,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        for name in names:
            self.assertIn(name, r.stdout)


if __name__ == "__main__":
    unittest.main()
