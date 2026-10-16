"""check_text_fit.sh keeps the Log window's widths with the one solver (#228)."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_text_fit.sh"

NAMES = ["log_row", "branch_pane_row", "branch_menu_row", "log_filter_chips"]


def markers(names=NAMES, body: str = "") -> str:
    return "".join(
        f"// FIT-ROW-BEGIN {n}\n{body}// FIT-ROW-END\n" for n in names
    )


def run_check(
    source: str,
    name: str = "ui/log.rs",
    with_markers: bool = True,
    extra: dict[str, str] | None = None,
) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "crates" / "desktop-native" / "src"
        (src / "ui").mkdir(parents=True)
        files = {name: source}
        if with_markers:
            files["ui/rows.rs"] = markers()
        files.update(extra or {})
        for rel, text in files.items():
            (src / rel).parent.mkdir(parents=True, exist_ok=True)
            (src / rel).write_text(text)
        return subprocess.run(
            ["bash", str(SCRIPT), tmp], capture_output=True, text=True, timeout=60
        )


class CheckTextFit(unittest.TestCase):
    def test_fitted_code_is_accepted(self):
        r = run_check(
            "fn row() { FitCell::new(id, text).solved(&fit, c).render(p, w); }\n"
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    # L1
    def test_a_flex_shrink_weight_is_rejected(self):
        r = run_check("fn f() { d.style().flex_shrink = Some(1000.); }\n")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("weight", r.stderr)

    def test_a_weight_in_any_product_file_is_rejected(self):
        r = run_check("fn f() {}\n", extra={"ui/header.rs": "fn f() { s.flex_shrink = Some(2.); }\n"})
        self.assertNotEqual(r.returncode, 0)

    def test_a_weight_in_test_code_is_not(self):
        src = "#[cfg(test)]\nmod row_fit_tests {\n    fn f() { s.flex_shrink = Some(1.); }\n}\n"
        self.assertEqual(run_check(src).returncode, 0)
        self.assertEqual(
            run_check("fn f() { s.flex_shrink = Some(1.); }\n", name="ui/log_tests.rs").returncode, 0
        )

    def test_a_cfg_test_helper_that_is_not_a_module_is_product_code(self):
        src = "#[cfg(test)]\nfn helper() { s.flex_shrink = Some(1.); }\n"
        self.assertNotEqual(run_check(src).returncode, 0)

    # L2
    def test_the_retired_names_are_rejected(self):
        for name in [
            "yielding_suffix", "LABEL_FLOOR_W", "MIN_LABELS_W",
            "MIN_SUBJECT_BESIDE_LABELS", "MIN_SUBJECT_W", "chip_text_cap",
            "CHIP_TEXT_MIN_W", "row_widths", "ref_labels_width",
        ]:
            with self.subTest(name=name):
                r = run_check(f"fn f() {{ {name}(1) }}\n")
                self.assertNotEqual(r.returncode, 0, name)

    def test_a_longer_name_that_holds_one_is_fine(self):
        self.assertEqual(run_check("fn my_row_widths_note() {}\nconst MIN_SUBJECT_WIDE: f32 = 1.;\n").returncode, 0)

    # L3
    def test_a_bare_ellipsis_in_the_log_files_is_rejected(self):
        for call in ["clip_text(x)", "fill_text(x)", "d.text_ellipsis()", "d.line_clamp(1)"]:
            for name in ["ui/log.rs", "ui/log_view.rs"]:
                with self.subTest(call=call, name=name):
                    r = run_check(f"fn f() {{ {call}; }}\n", name=name)
                    self.assertNotEqual(r.returncode, 0)
                    self.assertIn("FitCell", r.stderr)

    def test_a_reasoned_exemption_on_the_line_or_above_is_accepted(self):
        self.assertEqual(run_check("fn f() { clip_text(x); // FIT-EXEMPT: alone in its row\n}\n").returncode, 0)
        self.assertEqual(run_check("fn f() {\n    // FIT-EXEMPT: alone in its row\n    fill_text(x);\n}\n").returncode, 0)

    def test_an_empty_reason_is_rejected(self):
        self.assertNotEqual(run_check("fn f() { clip_text(x); // FIT-EXEMPT:\n}\n").returncode, 0)
        self.assertNotEqual(run_check("fn f() { clip_text(x); // FIT-EXEMPT:   \n}\n").returncode, 0)

    def test_fit_rs_and_other_files_are_not_checked_by_l3(self):
        for name in ["ui/fit.rs", "ui/header.rs", "ui/mod.rs"]:
            with self.subTest(name=name):
                self.assertEqual(run_check("fn f() { clip_text(x); }\n", name=name).returncode, 0)

    def test_l3_leaves_test_code_alone(self):
        src = "#[cfg(test)]\nmod row_fit_tests {\n    fn f() { clip_text(x); }\n}\n"
        self.assertEqual(run_check(src).returncode, 0)

    # L4
    def test_a_flexed_width_in_a_fitted_row_is_rejected(self):
        for token in [".flex_1()", ".flex_grow()", ".min_w_0()", ".flex_shrink(1.)", "clip_text(x)", "fill_text(x)"]:
            with self.subTest(token=token):
                inner = f"    d{token};\n"
                extra = {"ui/rows.rs": markers(NAMES[:1], inner) + markers(NAMES[1:])}
                r = run_check("fn f() {}\n", with_markers=False, extra=extra)
                self.assertNotEqual(r.returncode, 0)
                self.assertIn("solver", r.stderr)

    def test_an_exemption_does_not_help_inside_a_fitted_row(self):
        inner = "    d.flex_1(); // FIT-EXEMPT: no\n"
        extra = {"ui/rows.rs": markers(NAMES[:1], inner) + markers(NAMES[1:])}
        self.assertNotEqual(run_check("fn f() {}\n", with_markers=False, extra=extra).returncode, 0)

    def test_the_spacer_outside_a_region_is_fine(self):
        self.assertEqual(run_check("fn f() { d.flex_1().min_w_0(); }\n").returncode, 0)

    def test_flex_shrink_zero_is_not_flex_shrink(self):
        inner = "    d.flex_shrink_0();\n"
        extra = {"ui/rows.rs": markers(NAMES[:1], inner) + markers(NAMES[1:])}
        self.assertEqual(run_check("fn f() {}\n", with_markers=False, extra=extra).returncode, 0)

    # L5
    def test_a_missing_marker_fails(self):
        extra = {"ui/rows.rs": markers(NAMES[1:])}
        r = run_check("fn f() {}\n", with_markers=False, extra=extra)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("log_row", r.stderr)

    def test_a_marker_twice_fails(self):
        extra = {"ui/rows.rs": markers() + markers(NAMES[:1])}
        r = run_check("fn f() {}\n", with_markers=False, extra=extra)
        self.assertNotEqual(r.returncode, 0)

    def test_an_unclosed_marker_fails(self):
        extra = {"ui/rows.rs": markers(NAMES[1:]) + "// FIT-ROW-BEGIN log_row\nfn f() {}\n"}
        r = run_check("fn f() {}\n", with_markers=False, extra=extra)
        self.assertNotEqual(r.returncode, 0)

    def test_an_end_without_a_begin_fails(self):
        extra = {"ui/rows.rs": markers() + "// FIT-ROW-END\n"}
        self.assertNotEqual(run_check("fn f() {}\n", with_markers=False, extra=extra).returncode, 0)

    def test_the_real_tree_passes(self):
        root = Path(__file__).resolve().parents[2]
        r = subprocess.run(["bash", str(SCRIPT), str(root)], capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
