#!/usr/bin/env bash
set -euo pipefail

# Check for banned window.refresh() calls in test code within crates/desktop-native
# outside the explicit, commented allowlist marked with '// ALLOWED-TEST-REFRESH: <reason>'.
# Which lines are test code is decided by scripts/lib/test_code.sh, shared with
# check_msg_raw.sh.
#
# Usage: check_test_refresh.sh [ROOT]   (ROOT defaults to the repository root)

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
# shellcheck source=scripts/lib/test_code.sh
# shellcheck disable=SC1090,SC1091
source "$(dirname "$0")/lib/test_code.sh"

SRC="$ROOT/crates/desktop-native"
ERRORS=0

while IFS=: read -r file line_num line_content; do
	# Product code may refresh; only test code is held to the allowlist.
	is_test_line "$file" "$line_num" || continue

	# Check if preceding line contains ALLOWED-TEST-REFRESH:
	prev_line_num=$((line_num - 1))
	prev_line=$(sed -n "${prev_line_num}p" "$file")
	if [[ "$prev_line" != *"ALLOWED-TEST-REFRESH:"* ]]; then
		echo "ERROR: Unallowlisted .refresh() call in test code at ${file}:${line_num}:" >&2
		echo "  ${line_content}" >&2
		echo "  Preceding line: ${prev_line}" >&2
		echo "  Every .refresh() in test code must have an explicit '// ALLOWED-TEST-REFRESH: <reason>' comment." >&2
		ERRORS=$((ERRORS + 1))
	fi
done < <(grep -rn --include='*.rs' '\.refresh(' "$SRC" || true)

while IFS= read -r hit; do
	[[ -n "$hit" ]] || continue
	echo "ERROR: out-of-line test module not named *_tests.rs at ${hit}" >&2
	echo "  A whole-file test module must be named *_tests.rs, or the lints read it as product code." >&2
	ERRORS=$((ERRORS + 1))
done < <(misnamed_test_modules "$SRC/src")

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS test-code lint error(s)." >&2
	exit 1
fi

echo "check_test_refresh: all test .refresh() calls in crates/desktop-native are allowlisted with reasons."
exit 0
