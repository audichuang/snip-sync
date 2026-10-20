#!/usr/bin/env bash
set -euo pipefail

# A counter that decides whether a landing result is still wanted is a
# `Gen<Lane>` (crates/desktop-native/src/gen.rs): its stamp does not typecheck
# against another lane's counter, so "compare the wrong generation" does not
# compile (#230). This check keeps a bare `u64` out of the desktop's types: in
# the non-test code of crates/desktop-native/src, every `u64` token on a line (a field, a
# parameter, a return type, a `let` type, anywhere in a tuple, generic or
# array, a type alias, a `0u64` literal) and every `AtomicU64` has
# `// U64-NOT-GEN: <why>` on its line or the line above, and the reason is not
# empty. Not looked at: a cast (`n as u64`), a path (`u64::MAX`,
# `u64::from`) and a comment line. gen.rs is the one file that names the raw
# counter.
#
# Which lines are test code is decided by scripts/lib/test_code.sh, shared with
# check_test_refresh.sh and check_msg_raw.sh.
#
# Not covered: a counter of another width (`usize`, `u32`) and a name that
# hides the type (a macro). This is a net for a counter declared by accident,
# not a guard against one written to dodge it.
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

# A `u64` token: not part of a longer name; a digit before it is a literal
# suffix (`0u64`).
BARE_RE='(^|[^A-Za-z])u64([^A-Za-z0-9_]|$)|AtomicU64'
REASON_RE='//[[:space:]]*U64-NOT-GEN:[[:space:]]*[^[:space:]]'

while IFS=: read -r file line_num line_content; do
	[[ "$file" == "$SRC/gen.rs" ]] && continue
	# Comments say u64 in prose.
	[[ "$line_content" =~ ^[[:space:]]*// ]] && continue
	is_test_line "$file" "$line_num" && continue
	# A cast and a path are not declarations.
	stripped="${line_content//as u64/}"
	stripped="${stripped//u64::/}"
	[[ "$stripped" =~ $BARE_RE ]] || continue
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
done < <(grep -rnE --include='*.rs' -e "$BARE_RE" "$SRC" || true)

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: $ERRORS bare u64 declaration(s) in crates/desktop-native." >&2
	exit 1
fi

echo "check_generation_types: every u64 type in crates/desktop-native is a Gen or says why not."
exit 0
