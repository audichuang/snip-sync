"""check_test_refresh.sh holds every line of test code to the refresh rule (#233)."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_test_refresh.sh"
REPO = Path(__file__).resolve().parents[2]


def run_check(source: str, name: str) -> subprocess.CompletedProcess:
    # `cwd` is not a git repository: the script reads ROOT, not the checkout
    # it happens to run in.
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "crates" / "desktop-native" / "src"
        src.mkdir(parents=True)
        (src / name).write_text(source)
        return subprocess.run(
            ["bash", str(SCRIPT), tmp],
            capture_output=True,
            text=True,
            timeout=60,
            cwd=tmp,
        )


class CheckTestRefresh(unittest.TestCase):
    def test_a_whole_file_test_module_is_test_code(self):
        r = run_check("fn t() { window.refresh(); }\n", "foo_tests.rs")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("ALLOWED-TEST-REFRESH", r.stderr)

    def test_a_pub_crate_tests_block_is_test_code(self):
        src = "fn f() {}\npub(crate) mod tests {\n    fn t() { window.refresh(); }\n}\n"
        self.assertNotEqual(run_check(src, "lib.rs").returncode, 0)

    def test_the_allowance_comment_is_honoured(self):
        src = (
            "mod tests {\n    fn t() {\n"
            "        // ALLOWED-TEST-REFRESH: a reason\n"
            "        window.refresh();\n    }\n}\n"
        )
        r = run_check(src, "lib.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_product_code_may_refresh(self):
        r = run_check("fn f() { window.refresh(); }\n", "lib.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_an_out_of_line_test_module_must_be_named_tests(self):
        r = run_check("#[cfg(test)]\nmod helpers;\n", "lib.rs")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("_tests.rs", r.stderr)

    def test_real_tree_passes(self):
        r = subprocess.run(
            ["bash", str(SCRIPT)],
            capture_output=True,
            text=True,
            timeout=60,
            cwd=REPO,
        )
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
