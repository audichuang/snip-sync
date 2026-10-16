"""check_msg_raw.sh rejects the ways around Msg's typed wording (#223, #225)."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_msg_raw.sh"


def run_check(source: str, name: str = "lib.rs") -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "crates" / "desktop-native" / "src"
        src.mkdir(parents=True)
        (src / name).write_text(source)
        return subprocess.run(
            ["bash", str(SCRIPT), tmp], capture_output=True, text=True, timeout=60
        )


class CheckMsgRaw(unittest.TestCase):
    def test_typed_messages_are_accepted(self):
        r = run_check(
            "fn f() { Msg::new(\"error_open_repo\", [x]); Msg::detail(h, p) }\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_raw_call_is_rejected(self):
        self.assertNotEqual(run_check("fn f() { Msg::raw(s) }\n").returncode, 0)

    def test_an_allowance_comment_does_not_help(self):
        r = run_check("fn f() {\n    // ALLOWED-RAW: user text\n    Msg::raw(s)\n}\n")
        self.assertNotEqual(r.returncode, 0)

    def test_function_reference_is_rejected(self):
        r = run_check("fn f() { let g = Msg::raw; g(s) }\n")
        self.assertNotEqual(r.returncode, 0)
        r = run_check("fn f() { x.map(Msg::raw) }\n")
        self.assertNotEqual(r.returncode, 0)

    def test_self_and_qualified_self_raw_are_rejected(self):
        r = run_check("impl T for Msg { fn g() -> Self { Self::raw(\"x\") } }\n")
        self.assertNotEqual(r.returncode, 0)
        r = run_check("fn from(s: String) -> Self { <Self>::raw(s) }\n")
        self.assertNotEqual(r.returncode, 0)

    def test_raw_constructor_in_i18n_is_rejected(self):
        r = run_check("impl Msg {\n    pub fn raw(t: String) -> Self { todo!() }\n}\n", "i18n.rs")
        self.assertNotEqual(r.returncode, 0)

    def test_err_raw_key_is_rejected_everywhere(self):
        self.assertNotEqual(run_check('fn f() { Msg::new("err_raw", [s]) }\n').returncode, 0)
        self.assertNotEqual(run_check('let k = x.unwrap_or("err_raw");\n').returncode, 0)
        self.assertNotEqual(run_check('"err_raw" => "{}",\n', "i18n.rs").returncode, 0)

    def test_detail_keys_are_rejected_outside_i18n(self):
        for key in ("reason_detail", "remote_start_detail"):
            r = run_check(f'fn f() {{ Msg::new("{key}", [english, String::new()]) }}\n')
            self.assertNotEqual(r.returncode, 0, key)
            self.assertIn("Msg::detail", r.stderr)

    def test_detail_key_is_defined_in_i18n(self):
        r = run_check('"reason_detail" => "{0}: {1}",\n', "i18n.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_alias_is_rejected(self):
        r = run_check("use crate::i18n::Msg as Message;\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("aliased", r.stderr)
        for decl in ("type M = Msg;", "type M = crate::i18n::Msg;"):
            self.assertNotEqual(run_check(decl + "\n").returncode, 0, decl)

    def test_second_impl_block_is_rejected(self):
        r = run_check("impl Msg {\n    fn g() -> Self { todo!() }\n}\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("impl Msg", r.stderr)

    def test_test_module_is_ignored(self):
        r = run_check(
            "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { Msg::new(\"reason_detail\", [s]); }\n}\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_real_tree_passes(self):
        r = subprocess.run(
            ["bash", str(SCRIPT)], capture_output=True, text=True, timeout=60
        )
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
