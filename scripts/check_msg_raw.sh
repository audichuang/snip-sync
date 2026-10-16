#!/usr/bin/env bash
set -euo pipefail

# Guard the one way text reaches the zh-TW UI (#222, #225). A message is a
# dictionary key plus data; every failure is worded by `reason_msg`, and data
# that trails a headline goes through `Msg::detail`. `Msg::raw` is gone, so
# the compiler refuses it. This check rejects the ways around that, in the
# non-test code of crates/desktop-native/src:
#   - any reference to `Msg::raw` / `Self::raw` (call or function value), or a
#     `fn raw`, so the constructor cannot come back;
#   - the literal keys "err_raw", "reason_detail" and "remote_start_detail"
#     outside i18n.rs: `Msg::new("reason_detail", [english, ""])` would print
#     any string as it is, which is `raw` again under another name;
#   - aliasing Msg (`Msg as X`, `type X = Msg`) and `impl Msg` outside i18n.rs,
#     which would reach those constructors under a name this check cannot see.
# There is nothing to allow: a string with no wording of its own is a Reason.
#
# This catches a backdoor written by accident, not code written to dodge it
# (a macro that builds the name, say).
#
# Usage: check_msg_raw.sh [ROOT]   (ROOT defaults to the repository root)

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
SRC="$ROOT/crates/desktop-native/src"
ERRORS=0

if [[ ! -d "$SRC" ]]; then
	echo "ERROR: $SRC is not a directory" >&2
	exit 1
fi

I18N="$SRC/i18n.rs"
PATTERN='(Msg|Self)[>[:space:]]*::[[:space:]]*raw([^A-Za-z0-9_]|$)|fn[[:space:]]+raw([^A-Za-z0-9_]|$)|Msg[[:space:]]+as[[:space:]]|=[[:space:]]*([A-Za-z_0-9]+::)*Msg[[:space:]]*;|impl([[:space:]]*<[^>]*>)?[[:space:]]+Msg([^A-Za-z0-9_]|$)|"err_raw"|"reason_detail"|"remote_start_detail"'

reject() {
	echo "ERROR: $1 at ${file}:${line_num}: ${line_content}" >&2
	[[ -n "${2:-}" ]] && echo "  $2" >&2
	ERRORS=$((ERRORS + 1))
}

while IFS=: read -r file line_num line_content; do
	# Only the `mod tests` block counts as test code.
	tests_start=$(grep -nE '^(pub(\([a-z]+\))? )?mod tests \{' "$file" | head -1 | cut -d: -f1 || true)
	if [[ -n "$tests_start" ]] && [[ "$line_num" -ge "$tests_start" ]]; then
		continue
	fi

	if [[ "$line_content" =~ (Msg|Self)[\>[:space:]]*::[[:space:]]*raw([^A-Za-z0-9_]|$) ]] ||
		[[ "$line_content" =~ fn[[:space:]]+raw([^A-Za-z0-9_]|$) ]]; then
		reject "raw message constructor" "Msg::raw is gone: give the text a Reason (reason_msg), or trail it as data with Msg::detail."
		continue
	fi
	if [[ "$line_content" =~ \"err_raw\" ]]; then
		reject "the err_raw key" "A string with no wording of its own is a Reason, not a message."
		continue
	fi
	if [[ "$file" == "$I18N" ]]; then
		# i18n.rs defines the keys, their wording and the builders.
		continue
	fi
	if [[ "$line_content" =~ \"reason_detail\"|\"remote_start_detail\" ]]; then
		reject "detail key outside i18n.rs" "Build it with Msg::detail(headline, datum): the key prints any string as it is."
		continue
	fi
	if [[ "$line_content" =~ Msg[[:space:]]+as[[:space:]] ]] ||
		[[ "$line_content" =~ =[[:space:]]*([A-Za-z_0-9]+::)*Msg[[:space:]]*\; ]]; then
		reject "Msg is aliased" "Do not alias Msg: this check cannot see a constructor under another name."
		continue
	fi
	if [[ "$line_content" =~ impl([[:space:]]*\<[^\>]*\>)?[[:space:]]+Msg([^A-Za-z0-9_]|$) ]]; then
		reject "impl Msg outside i18n.rs" "A second impl block could define a raw-text constructor this check cannot see."
		continue
	fi
done < <(grep -rnE --include='*.rs' "$PATTERN" "$SRC" || true)

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS raw-text backdoor(s) in desktop product code." >&2
	exit 1
fi

echo "check_msg_raw: no raw-text message path in crates/desktop-native/src product code."
exit 0
