#!/usr/bin/env bash
set -euo pipefail

# A counter that decides whether a landing result is still wanted is a
# `Gen<Lane>` (crates/desktop-native/src/gen.rs): its stamp does not typecheck
# against another lane's counter, so "compare the wrong generation" does not
# compile (#230). This check keeps a bare `u64` out of the desktop's types: in
# the non-test code of crates/desktop-native/src, a `u64` in a type position (a
# field, a parameter, a return type, a `let` type, inside a tuple or a
# generic, or opening a continuation line of a type that spans lines) has
# `// U64-NOT-GEN: <why>` on its line or the line above, and the reason is not
# empty. A cast (`n as u64`) is not a declaration and is not looked at; gen.rs
# is the one file that names the raw counter.
#
# Which lines are test code is decided by scripts/lib/test_code.sh, shared with
# check_test_refresh.sh and check_msg_raw.sh.
#
# This catches a counter declared by accident as a bare integer, not one
# written to dodge it (a type alias, a macro).
#
# Usage: check_generation_types.sh [ROOT]   (ROOT defaults to the repository root)

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

END='([,>);=[:space:]]|$)'
WRAP='(mut[[:space:]]+)?([A-Za-z_:]+<|\()*'
TYPE_RE="(:|->)[[:space:]]*&?[[:space:]]*${WRAP}u64${END}"
# A type that spans lines continues with the wrappers still open.
CONT_RE="^[[:space:]]*${WRAP}u64${END}"
REASON_RE='//[[:space:]]*U64-NOT-GEN:[[:space:]]*[^[:space:]]'

while IFS=: read -r file line_num line_content; do
	[[ "$file" == "$SRC/gen.rs" ]] && continue
	# Comments say u64 in prose.
	[[ "$line_content" =~ ^[[:space:]]*// ]] && continue
	is_test_line "$file" "$line_num" && continue
	if [[ "$line_content" =~ $REASON_RE ]]; then
		continue
	fi
	prev_line=$(line_before "$file" "$line_num")
	if [[ "$prev_line" =~ $REASON_RE ]]; then
		continue
	fi
	echo "ERROR: a bare u64 at ${file}:${line_num}: ${line_content}" >&2
	echo "  A counter a landing result is compared with is a Gen<Lane> (gen.rs)." >&2
	echo "  Anything else says so: '// U64-NOT-GEN: <why>' on this line or the one above." >&2
	ERRORS=$((ERRORS + 1))
done < <(grep -rnE --include='*.rs' -e "$TYPE_RE" -e "$CONT_RE" "$SRC" || true)

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: $ERRORS bare u64 declaration(s) in crates/desktop-native." >&2
	exit 1
fi

echo "check_generation_types: every u64 type in crates/desktop-native is a Gen or says why not."
exit 0
