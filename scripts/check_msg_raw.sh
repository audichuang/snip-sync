#!/usr/bin/env bash
set -euo pipefail

# Check how a raw message can be built in the non-test code of
# crates/desktop-native/src. A raw message shows as it is in either language,
# so an English literal there leaks into the zh-TW UI (#222). Each of these
# needs '// ALLOWED-RAW: <reason>' (a non-empty reason) on the same or the
# previous line:
#   - any reference to `Msg::raw` / `Self::raw` (call or function value);
#   - the literal key "err_raw" outside i18n.rs (`Msg::new("err_raw", ..)`).
# These are rejected outright, since they reach `raw` under a name or in a
# place this check does not look: aliasing Msg (`Msg as X`, `type X = Msg`) and
# `impl Msg` outside i18n.rs.
#
# This catches raw messages written by accident, not code written to dodge it
# (a macro that builds the name, say). Removing Msg::raw itself (#225) is what
# makes the compiler refuse them.
#
# Usage: check_msg_raw.sh [ROOT]   (ROOT defaults to the repository root)

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
SRC="$ROOT/crates/desktop-native/src"
ERRORS=0

if [[ ! -d "$SRC" ]]; then
	echo "ERROR: $SRC is not a directory" >&2
	exit 1
fi

allowed() {
	[[ "$1" =~ ALLOWED-RAW:[[:space:]]*[^[:space:]] ]]
}

I18N="$SRC/i18n.rs"
PATTERN='(Msg|Self)[>[:space:]]*::[[:space:]]*raw([^A-Za-z0-9_]|$)|Msg[[:space:]]+as[[:space:]]|=[[:space:]]*([A-Za-z_0-9]+::)*Msg[[:space:]]*;|impl([[:space:]]*<[^>]*>)?[[:space:]]+Msg([^A-Za-z0-9_]|$)|"err_raw"'

reject() {
	echo "ERROR: $1 at ${file}:${line_num}: ${line_content}" >&2
	[[ -n "${2:-}" ]] && echo "  $2" >&2
	ERRORS=$((ERRORS + 1))
}

while IFS=: read -r file line_num line_content; do
	# Only the `mod tests` block counts as test code; it may build raw messages.
	tests_start=$(grep -nE '^(pub(\([a-z]+\))? )?mod tests \{' "$file" | head -1 | cut -d: -f1 || true)
	if [[ -n "$tests_start" ]] && [[ "$line_num" -ge "$tests_start" ]]; then
		continue
	fi

	if [[ "$line_content" =~ Msg[[:space:]]+as[[:space:]] ]] ||
		[[ "$line_content" =~ =[[:space:]]*([A-Za-z_0-9]+::)*Msg[[:space:]]*\; ]]; then
		reject "Msg is aliased" "Do not alias Msg: check_msg_raw cannot see Msg::raw under another name."
		continue
	fi
	if [[ "$file" != "$I18N" ]] &&
		[[ "$line_content" =~ impl([[:space:]]*\<[^\>]*\>)?[[:space:]]+Msg([^A-Za-z0-9_]|$) ]]; then
		reject "impl Msg outside i18n.rs" "Self::raw would be reachable from a second impl block."
		continue
	fi
	if [[ "$file" == "$I18N" ]] && [[ ! "$line_content" =~ (Msg|Self)[\>[:space:]]*::[[:space:]]*raw ]]; then
		# i18n.rs defines the key and its wording.
		continue
	fi
	prev_line=""
	if [[ "$line_num" -gt 1 ]]; then
		prev_line=$(sed -n "$((line_num - 1))p" "$file")
	fi
	if ! allowed "$line_content" && ! allowed "$prev_line"; then
		echo "ERROR: Unallowlisted raw message construction in product code at ${file}:${line_num}:" >&2
		echo "  ${line_content}" >&2
		echo "  Preceding line: ${prev_line}" >&2
		echo "  Use a translated key, or add '// ALLOWED-RAW: <reason>' (reason required) on the same or previous line." >&2
		ERRORS=$((ERRORS + 1))
	fi
done < <(grep -rnE --include='*.rs' "$PATTERN" "$SRC" || true)

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS unallowlisted, aliased or out-of-place raw message construction(s)." >&2
	exit 1
fi

echo "check_msg_raw: every raw message construction in crates/desktop-native/src product code is allowlisted with a reason."
exit 0
