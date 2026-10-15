"""check_msg_raw.sh rejects Msg::raw without a reason, aliased or by reference (#223)."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_msg_raw.sh"


def run_check(source: str) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "crates" / "desktop-native" / "src"
        src.mkdir(parents=True)
        (src / "lib.rs").write_text(source)
        return subprocess.run(
            ["bash", str(SCRIPT), tmp], capture_output=True, text=True, timeout=60
        )


class CheckMsgRaw(unittest.TestCase):
    def test_annotated_call_is_accepted(self):
        r = run_check(
            "fn f() {\n    // ALLOWED-RAW: user text\n    Msg::raw(s)\n}\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_same_line_annotation_is_accepted(self):
        r = run_check("fn f() { Msg::raw(s) } // ALLOWED-RAW: path\n")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_bare_call_is_rejected(self):
        self.assertNotEqual(run_check("fn f() { Msg::raw(s) }\n").returncode, 0)

    def test_empty_reason_is_rejected(self):
        for marker in ("// ALLOWED-RAW:", "// ALLOWED-RAW:   "):
            r = run_check(f"fn f() {{\n    {marker}\n    Msg::raw(s)\n}}\n")
            self.assertNotEqual(r.returncode, 0, marker)

    def test_alias_is_rejected_even_when_annotated(self):
        r = run_check(
            "use crate::i18n::Msg as Message;\n"
            "fn f() {\n    // ALLOWED-RAW: x\n    Message::raw(s)\n}\n"
        )
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("aliased", r.stderr)

    def test_function_reference_is_rejected(self):
        r = run_check("fn f() { let g = Msg::raw; g(s) }\n")
        self.assertNotEqual(r.returncode, 0)
        r = run_check("fn f() { x.map(Msg::raw) }\n")
        self.assertNotEqual(r.returncode, 0)

    def test_annotated_function_reference_is_accepted(self):
        r = run_check("fn f() {\n    // ALLOWED-RAW: names\n    x.map(Msg::raw)\n}\n")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_type_alias_is_rejected(self):
        for decl in ("type M = Msg;", "type M = crate::i18n::Msg;"):
            r = run_check(
                f"{decl}\nfn f() {{\n    // ALLOWED-RAW: x\n    M::raw(s)\n}}\n"
            )
            self.assertNotEqual(r.returncode, 0, decl)

    def test_second_impl_block_is_rejected(self):
        r = run_check(
            "impl Msg {\n    fn g() -> Self {\n        // ALLOWED-RAW: x\n"
            "        Self::raw(\"x\")\n    }\n}\n"
        )
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("impl Msg", r.stderr)

    def test_self_raw_in_trait_impl_needs_allowance(self):
        r = run_check("impl T for Msg { fn g() -> Self { Self::raw(\"x\") } }\n")
        self.assertNotEqual(r.returncode, 0)

    def test_qualified_self_raw_needs_allowance(self):
        r = run_check("impl From<String> for Msg { fn from(s: String) -> Self { <Self>::raw(s) } }\n")
        self.assertNotEqual(r.returncode, 0)

    def test_err_raw_key_needs_allowance(self):
        src = 'fn f() { Msg::new("err_raw", [s]) }\n'
        self.assertNotEqual(run_check(src).returncode, 0)
        self.assertNotEqual(run_check('let k = x.unwrap_or("err_raw");\n').returncode, 0)
        ok = '// ALLOWED-RAW: path\nfn f() { Msg::new("err_raw", [s]) }\n'
        self.assertEqual(run_check(ok).returncode, 0)

    def test_test_module_is_ignored(self):
        r = run_check(
            "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { Msg::raw(s); }\n}\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_real_tree_passes(self):
        r = subprocess.run(
            ["bash", str(SCRIPT)], capture_output=True, text=True, timeout=60
        )
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
