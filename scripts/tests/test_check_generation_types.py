"""check_generation_types.sh keeps a bare u64 out of the desktop's state (#230).

A counter that a landing result is compared with is a `Gen<Lane>`, so a stamp
of one lane does not typecheck against another. A `u64` in a type position is
either that counter (convert it) or something else, and says so on its line or
the line above: `// U64-NOT-GEN: <why>`.
"""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_generation_types.sh"
REPO = Path(__file__).resolve().parents[2]


def run_check(source: str, name: str = "lib.rs") -> subprocess.CompletedProcess:
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


class CheckGenerationTypes(unittest.TestCase):
    def test_a_field_with_no_counter_name_is_refused(self):
        # What the name says is not the rule: any u64 needs a decision.
        r = run_check("struct S {\n    log_reload_for: Option<u64>,\n}\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("U64-NOT-GEN", r.stderr)
        self.assertIn("lib.rs:2", r.stderr)

    def test_the_line_above_may_carry_the_reason(self):
        src = "struct S {\n    // U64-NOT-GEN: id\n    id: u64,\n}\n"
        r = run_check(src)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_the_same_line_may_carry_the_reason(self):
        r = run_check("struct S { id: u64 } // U64-NOT-GEN: id\n")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_cast_is_not_a_declaration(self):
        r = run_check("fn f(n: usize) -> S { S { count: count as u64 } }\n")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_return_type_is_a_declaration(self):
        r = run_check("fn f() -> u64 {\n    0\n}\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("lib.rs:1", r.stderr)

    def test_gen_rs_is_the_one_file_that_names_the_raw_counter(self):
        r = run_check("pub(crate) struct Gen<L> { n: u64 }\n", "gen.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_whole_file_test_module_is_test_code(self):
        r = run_check("fn t() -> u64 { 0 }\n", "x_tests.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_reason_must_say_something(self):
        r = run_check("struct S {\n    // U64-NOT-GEN:\n    id: u64,\n}\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("lib.rs:3", r.stderr)

    def test_a_type_spread_over_lines_is_seen(self):
        src = (
            "struct S {\n"
            "    cache: std::cell::RefCell<\n"
            "        Option<(u64, std::rc::Rc<Vec<u8>>)>,\n"
            "    >,\n"
            "}\n"
        )
        r = run_check(src)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("lib.rs:3", r.stderr)

    def test_a_cfg_test_block_is_test_code(self):
        src = "#[cfg(test)]\nmod tests {\n    fn t() -> u64 { 0 }\n}\nfn p() {}\n"
        r = run_check(src)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_product_code_after_a_test_block_is_not(self):
        src = "#[cfg(test)]\nmod tests {\n    fn t() -> u64 { 0 }\n}\nfn p() -> u64 { 0 }\n"
        r = run_check(src)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("lib.rs:5", r.stderr)
        self.assertNotIn("lib.rs:3", r.stderr)

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
