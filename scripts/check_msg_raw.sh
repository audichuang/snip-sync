#!/usr/bin/env bash
set -euo pipefail

# Guard the one way text reaches the zh-TW UI (#222, #225, #233). A message is
# a dictionary key plus data; every failure is worded by `reason_msg`, and
# data that trails a headline goes through `Msg::detail`. `Msg::raw` is gone,
# so the compiler refuses it. This check rejects the ways around that, in the
# non-test code of crates/desktop-native/src:
#   - any reference to `Msg::raw` / `Self::raw` (call or function value), or a
#     `fn raw`, so the constructor cannot come back;
#   - the literal keys "err_raw", "reason_detail" and "remote_start_detail"
#     outside i18n.rs: `Msg::new("reason_detail", [english, ""])` would print
#     any string as it is, which is `raw` again under another name;
#   - aliasing Msg (`Msg as X`, `type X = Msg`) and `impl Msg` outside i18n.rs,
#     which would reach those constructors under a name this check cannot see;
#   - `Msg::detail(` outside reason.rs and i18n.rs: a datum trails a headline
#     only where its source is typed (a Reason, or a translation function of
#     reason.rs);
#   - an error's `Display` on its way to the screen: `e.to_string()`, or
#     `{e}` / `{err}` / `{error}` inside a `format!(`, or one of them as a
#     positional argument (`format!("..{}", e)`) on the same line. Word the error by its
#     Reason, or by a typed translation (`graph_error_msg`, `clip_fault_msg`).
#     A line of an `[APP:...]` log (`app_log!`, `println!` ...) may print it;
#     `// ALLOWED-ERR-TEXT: <reason>` on the line or the one above allows the
#     rest (the COPY_COMMITS_ERR e2e contract).
# Over crates/{core,remote,cli,desktop-native}/src:
#   - `Datum::git_stderr`, `git_output`, `transport_stderr` and
#     `old_peer_text` take no string literal: text a producer wrote is not
#     what git, ssh or an old peer said;
#   - a whole-file test module (`#[cfg(test)]` then `mod x;`) is named
#     `*_tests`, so the test-code rule below sees it.
# Which lines are test code is decided by scripts/lib/test_code.sh (a
# `*_tests.rs` file, or a top-level `#[cfg(test)] mod NAME {` block), shared
# with check_test_refresh.sh. A string with no wording of its own is a Reason: there
# is nothing to allow for the first rules.
#
# This catches a backdoor written by accident, not code written to dodge it
# (a macro that builds the name, say). The error rule reads one line: an
# error under another name (`why`, `cause`), or a `format!` spread over
# several lines, is not seen.
#
# Usage: check_msg_raw.sh [ROOT]   (ROOT defaults to the repository root)

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

I18N="$SRC/i18n.rs"
PATTERN='(Msg|Self)[>[:space:]]*::[[:space:]]*raw([^A-Za-z0-9_]|$)|fn[[:space:]]+raw([^A-Za-z0-9_]|$)|Msg[[:space:]]+as[[:space:]]|=[[:space:]]*([A-Za-z_0-9]+::)*Msg[[:space:]]*;|impl([[:space:]]*<[^>]*>)?[[:space:]]+Msg([^A-Za-z0-9_]|$)|"err_raw"|"reason_detail"|"remote_start_detail"'

reject() {
	echo "ERROR: $1 at ${file}:${line_num}: ${line_content}" >&2
	[[ -n "${2:-}" ]] && echo "  $2" >&2
	ERRORS=$((ERRORS + 1))
}

while IFS=: read -r file line_num line_content; do
	is_test_line "$file" "$line_num" && continue

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

# `Msg::detail` outside reason.rs and i18n.rs, and an error's Display going to
# the screen (desktop product code).
while IFS=: read -r file line_num line_content; do
	is_test_line "$file" "$line_num" && continue
	case "$file" in
	"$SRC/reason.rs" | "$I18N") ;;
	*)
		if [[ "$line_content" =~ Msg[[:space:]]*::[[:space:]]*detail[[:space:]]*\( ]]; then
			reject "Msg::detail outside reason.rs" "Data trails a headline only through reason.rs: give it a Reason, or add a typed translation function there."
			continue
		fi
		;;
	esac
	if [[ "$line_content" =~ (^|[^A-Za-z0-9_])(e|err|error)\.to_string\(\) ]] ||
		{
			[[ "$line_content" =~ format!\( ]] &&
				{
					[[ "$line_content" =~ \{(e|err|error)(:[^\}]*)?\} ]] ||
						[[ "$line_content" =~ \{[^\}]*\}.*,[[:space:]]*(e|err|error)[[:space:]]*[,\)] ]]
				}
		}; then
		if [[ "$line_content" =~ app_log!|root_log!|println!|eprintln! ]]; then
			continue
		fi
		if [[ "$line_content" == *"ALLOWED-ERR-TEXT:"* ]]; then
			continue
		fi
		prev=$(sed -n "$((line_num - 1))p" "$file")
		if [[ "$prev" == *"ALLOWED-ERR-TEXT:"* ]]; then
			continue
		fi
		reject "an error's Display on its way to the screen" "Word it by its Reason (reason_msg) or a typed translation (graph_error_msg, clip_fault_msg). Only an [APP:...] log line may print it; mark an e2e contract with // ALLOWED-ERR-TEXT: <reason>."
	fi
done < <(grep -rnE --include='*.rs' 'Msg[[:space:]]*::[[:space:]]*detail[[:space:]]*\(|(^|[^A-Za-z0-9_])(e|err|error)\.to_string\(\)|format!\(.*\{(e|err|error)(:[^}]*)?\}|format!\(.*\{[^}]*\}.*,[[:space:]]*(e|err|error)[[:space:]]*[,)]' "$SRC" || true)

# Text an external source wrote is not a string literal of ours.
for crate in core remote cli desktop-native; do
	dir="$ROOT/crates/$crate/src"
	[[ -d "$dir" ]] || continue
	while IFS=: read -r file line_num line_content; do
		is_test_line "$file" "$line_num" && continue
		reject "a Datum built from a literal" "git_stderr, git_output, transport_stderr and old_peer_text hold what git, ssh or an old peer said: pass that text, not one of ours (Datum::label for a fixed bug-report label)."
	done < <(grep -rnE --include='*.rs' 'Datum[[:space:]]*::[[:space:]]*(git_stderr|git_output|transport_stderr|old_peer_text)[[:space:]]*\([[:space:]]*"' "$dir" || true)
done

while IFS= read -r hit; do
	[[ -n "$hit" ]] || continue
	echo "ERROR: out-of-line test module not named *_tests.rs at ${hit}" >&2
	echo "  A whole-file test module must be named *_tests.rs, or the lints read it as product code." >&2
	ERRORS=$((ERRORS + 1))
done < <(misnamed_test_modules "$ROOT/crates/core/src" "$ROOT/crates/remote/src" "$ROOT/crates/cli/src" "$SRC")

if [ "$ERRORS" -ne 0 ]; then
	echo "FAILED: Found $ERRORS message-path violation(s) in product code." >&2
	exit 1
fi

echo "check_msg_raw: no raw-text message path in product code."
exit 0
