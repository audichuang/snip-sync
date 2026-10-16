#!/usr/bin/env bash
set -euo pipefail

# Guard the one rule for how a text of the Log window gives way (#228, ADR
# 0005). A row's widths come from `fit_row` and the rule table
# (`FitRole::rule`), and every text is drawn by a `FitCell`; a weight on a
# flex item, a bare ellipsis or a second solver would bring the old circle of
# rules back. In the non-test code of crates/desktop-native/src this rejects:
#   L1  `flex_shrink = Some(`: dividing a width by weight;
#   L2  the names of the retired solver (`yielding_suffix`, `LABEL_FLOOR_W`,
#       `MIN_LABELS_W`, `MIN_SUBJECT_BESIDE_LABELS`, `MIN_SUBJECT_W`,
#       `chip_text_cap`, `CHIP_TEXT_MIN_W`, `row_widths`, `ref_labels_width`);
#   L3  in ui/log.rs and ui/log_view.rs, `clip_text(`, `fill_text(`,
#       `.text_ellipsis()` and `.line_clamp(` that give a text an ellipsis
#       outside a `FitCell`, unless the line or the one above has
#       `// FIT-EXEMPT: <reason>` (a text that shares its width with nobody);
#   L4  between `// FIT-ROW-BEGIN <name>` and `// FIT-ROW-END`: `.flex_1()`,
#       `.flex_grow()`, `.min_w_0()`, `.flex_shrink(`, `clip_text(` and
#       `fill_text(`, and no exemption helps: the widths in such a row are the
#       solver's;
#   L5  each of the four markers `log_row`, `branch_pane_row`,
#       `branch_menu_row` and `log_filter_chips` exactly once, each closed by
#       a `FIT-ROW-END`, so deleting a marker does not get a row out of L4.
# Which lines are test code is decided by scripts/lib/test_code.sh, shared
# with check_msg_raw.sh and check_test_refresh.sh. ui/fit.rs, where the
# ellipsis and the one spacer live, is not checked by L3.
#
# Usage: check_text_fit.sh [ROOT]   (ROOT defaults to the repository root)

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
# shellcheck source=scripts/lib/test_code.sh
# shellcheck disable=SC1090,SC1091
source "$(dirname "$0")/lib/test_code.sh"
SRC="$ROOT/crates/desktop-native/src"
ERRORS=0

if [[ ! -d "$SRC" ]]; then
	echo "ERROR: $SRC is not a directory" >&2
	exit 1
fi

reject() {
	echo "ERROR: $1 at ${file}:${line_num}: ${line_content}" >&2
	[[ -n "${2:-}" ]] && echo "  $2" >&2
	ERRORS=$((ERRORS + 1))
}

# A reason after FIT-EXEMPT: is more than a colon.
exempt() { # TEXT
	[[ "$1" =~ FIT-EXEMPT:[[:space:]]*[^[:space:]] ]]
}

# L1, L2
L2_NAMES='yielding_suffix|LABEL_FLOOR_W|MIN_LABELS_W|MIN_SUBJECT_BESIDE_LABELS|MIN_SUBJECT_W|chip_text_cap|CHIP_TEXT_MIN_W|row_widths|ref_labels_width'
while IFS=: read -r file line_num line_content; do
	is_test_line "$file" "$line_num" && continue
	if [[ "$line_content" =~ flex_shrink[[:space:]]*=[[:space:]]*Some\( ]]; then
		reject "a weight on a flex item" "Widths are the solver's (fit_row): give the text a role in FitRole and a claim, do not weight it."
	else
		reject "a name of the retired width solver" "Row widths come from fit_row and the rule table (docs/adr/0005-one-fit-rule-table.md)."
	fi
done < <(grep -rnE --include='*.rs' "flex_shrink[[:space:]]*=[[:space:]]*Some\(|(^|[^A-Za-z0-9_])($L2_NAMES)([^A-Za-z0-9_]|\$)" "$SRC" || true)

# L3
for rel in ui/log.rs ui/log_view.rs; do
	file="$SRC/$rel"
	[[ -f "$file" ]] || continue
	while IFS=: read -r line_num line_content; do
		is_test_line "$file" "$line_num" && continue
		exempt "$line_content" && continue
		prev=$(line_before "$file" "$line_num")
		exempt "$prev" && continue
		reject "an ellipsis outside a FitCell" "Draw the text with FitCell (ui/fit.rs), or say why it shares its width with nobody: // FIT-EXEMPT: <reason>."
	done < <(grep -nE '(clip_text|fill_text)\(|\.text_ellipsis\(\)|\.line_clamp\(' "$file" || true)
done

# L4, L5: the regions between the markers. `marks` holds one line per
# FIT-ROW-BEGIN in product code, its name.
marks=""
ends=0
# is_test_line reads the file this loop is reading; neither writes it.
# shellcheck disable=SC2094
while IFS= read -r file; do
	in_region=""
	line_num=0
	while IFS= read -r line_content || [[ -n "$line_content" ]]; do
		line_num=$((line_num + 1))
		case "$line_content" in
		*"FIT-ROW-BEGIN "*)
			is_test_line "$file" "$line_num" && continue
			name="${line_content##*FIT-ROW-BEGIN }"
			name="${name%%[[:space:]]*}"
			marks="${marks}${name}"$'\n'
			if [[ -n "$in_region" ]]; then
				reject "FIT-ROW-BEGIN inside the region ${in_region}" "Close a region with // FIT-ROW-END before the next begins."
			fi
			in_region="$name"
			continue
			;;
		*"// FIT-ROW-END"*)
			is_test_line "$file" "$line_num" && continue
			ends=$((ends + 1))
			if [[ -z "$in_region" ]]; then
				reject "FIT-ROW-END without a FIT-ROW-BEGIN" ""
			fi
			in_region=""
			continue
			;;
		esac
		[[ -n "$in_region" ]] || continue
		if [[ "$line_content" =~ \.flex_1\(\)|\.flex_grow\(\)|\.min_w_0\(\)|\.flex_shrink\(|clip_text\(|fill_text\( ]]; then
			reject "a width the solver does not know, in row ${in_region}" "A row between FIT-ROW-BEGIN and FIT-ROW-END takes its widths from fit_row: no flex_1, flex_grow, min_w_0, flex_shrink(..), clip_text or fill_text, and nothing exempts them."
		fi
	done <"$file"
	if [[ -n "$in_region" ]]; then
		line_content=""
		reject "FIT-ROW-BEGIN ${in_region} is never closed" "Add // FIT-ROW-END."
	fi
done < <(grep -rlE --include='*.rs' 'FIT-ROW-(BEGIN|END)' "$SRC" | sort || true)

total=0
for name in log_row branch_pane_row branch_menu_row log_filter_chips; do
	n=$(printf '%s' "$marks" | grep -cxF "$name" || true)
	total=$((total + n))
	if [[ "$n" -ne 1 ]]; then
		file="$SRC"
		line_num=0
		line_content="FIT-ROW-BEGIN $name ($n times)"
		reject "the marker ${name} must appear exactly once" "Without it the row is out of L4's reach."
	fi
done
if [[ "$ends" -ne "$total" && "$ERRORS" -eq 0 ]]; then
	file="$SRC"
	line_num=0
	line_content="$ends FIT-ROW-END for $total markers"
	reject "FIT-ROW-BEGIN and FIT-ROW-END do not pair up" ""
fi

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS text-fit violation(s) in product code." >&2
	exit 1
fi

echo "check_text_fit: no weighted width, no bare ellipsis, the fitted rows are marked."
exit 0
