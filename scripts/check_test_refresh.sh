#!/usr/bin/env bash
set -euo pipefail

# Check for banned window.refresh() calls in test code within crates/desktop-native
# outside the explicit, commented allowlist marked with '// ALLOWED-TEST-REFRESH: <reason>'.

ERRORS=0

while IFS=: read -r file line_num line_content; do
	# Skip known product code calls (sync_appearance and log_view deferred refresh)
	if [[ "$file" == *"src/ui/log_view.rs" ]] && [[ "$line_content" == *"update(cx, |_, w, _| w.refresh())"* ]]; then
		continue
	fi
	if [[ "$file" == *"src/main.rs" ]] && [[ "$line_num" -le 5800 ]] && [[ "$line_content" == *"window.refresh();"* ]]; then
		continue
	fi

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
done < <(git grep -n "\.refresh(" crates/desktop-native/)

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS unallowlisted test .refresh() call(s)." >&2
	exit 1
fi

echo "check_test_refresh: all test .refresh() calls in crates/desktop-native are allowlisted with reasons."
exit 0
