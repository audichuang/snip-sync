"""check_msg_raw.sh rejects the ways around Msg's typed wording (#223, #225)."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_msg_raw.sh"


def run_check(
    source: str, name: str = "lib.rs", crate: str = "desktop-native"
) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        # The desktop crate's source always exists (the script requires it);
        # `crate` is where the file under test lives.
        (Path(tmp) / "crates" / "desktop-native" / "src").mkdir(parents=True)
        src = Path(tmp) / "crates" / crate / "src"
        src.mkdir(parents=True, exist_ok=True)
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

    def test_msg_detail_belongs_to_reason_rs_and_i18n_rs(self):
        # (a) anywhere else the datum could be a sentence of ours.
        r = run_check('fn f() { Msg::detail(h, e) }\n', "history.rs")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("reason.rs", r.stderr)
        # (b) reason.rs words data; i18n.rs defines the builder.
        r = run_check('fn f() { Msg::detail(h, e) }\n', "reason.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_an_errors_display_does_not_reach_the_screen(self):
        # (c)
        r = run_check('fn f() { set_status("k", [e.to_string()]) }\n')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("ALLOWED-ERR-TEXT", r.stderr)
        # (d) the log marker, on the line or the one above.
        r = run_check('fn f() { x(e.to_string()) } // ALLOWED-ERR-TEXT: log\n')
        self.assertEqual(r.returncode, 0, r.stderr)
        r = run_check(
            "fn f() {\n    // ALLOWED-ERR-TEXT: e2e contract\n    x(e.to_string())\n}\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        # (h) an interpolated error in a message, not in a log line.
        r = run_check('fn f() { Msg::new("k", [format!("{e}")]) }\n')
        self.assertNotEqual(r.returncode, 0)
        r = run_check('fn f() { app_log!("[APP:X: {err}]") }\n')
        self.assertEqual(r.returncode, 0, r.stderr)
        # A name that merely ends in `e` is not an error value.
        r = run_check('fn f() { x(name.to_string()) }\n')
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_test_code_is_exempt_from_the_new_rules(self):
        # (e) after `mod tests {`; (f) a whole `*_tests.rs` file.
        src = "fn f() {}\nmod tests {\n    fn t() { x(e.to_string()); Msg::detail(h, d) }\n}\n"
        r = run_check(src)
        self.assertEqual(r.returncode, 0, r.stderr)
        r = run_check("fn t() { x(e.to_string()); Msg::detail(h, d) }\n", "foo_tests.rs")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_datum_from_outside_takes_no_literal(self):
        # (g)
        r = run_check('fn f() { Datum::git_stderr("boom") }\n', crate="remote")
        self.assertNotEqual(r.returncode, 0)
        r = run_check('fn f() { Datum::old_peer_text( "x") }\n', crate="core")
        self.assertNotEqual(r.returncode, 0)
        r = run_check('fn f() { Datum::git_stderr(stderr) }\n', crate="remote")
        self.assertEqual(r.returncode, 0, r.stderr)
        # `label` and `command` are the allowed spellings of fixed text.
        r = run_check('fn f() { Datum::label("x") }\n', crate="remote")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_whole_file_test_module_is_named_tests(self):
        # (i)
        r = run_check("#[cfg(test)]\nmod helpers;\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("_tests.rs", r.stderr)
        r = run_check("#[cfg(test)]\nmod helpers_tests;\n")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_real_tree_passes(self):
        r = subprocess.run(
            ["bash", str(SCRIPT)], capture_output=True, text=True, timeout=60
        )
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
