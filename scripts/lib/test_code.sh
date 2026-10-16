# shellcheck shell=bash
# Which lines of a Rust file are test code, for every lint that holds product
# code to a rule tests may break (check_test_refresh.sh, check_msg_raw.sh).
# One rule, so the lints cannot disagree:
#   - a file named `*_tests.rs` is a test module, whole;
#   - otherwise test code runs from the first top-level `mod tests {`
#     (any visibility) to the end of the file.
# A file-level test module named otherwise would read as product code, so
# misnamed_test_modules reports `#[cfg(test)] mod x;` whose x is not `*_tests`.
# Limit: it sees only `#[cfg(test)]` on the line right above `mod x;`. The
# one-line form or an attribute in between (`#[path = ...]`) is not seen; write
# out-of-line test modules in the plain two-line form.

test_code_start() { # FILE -> first line of test code, or nothing
	case "$1" in
	*_tests.rs) echo 1 ;;
	*) grep -nE '^(pub(\([a-z]+\))? )?mod tests \{' "$1" | head -1 | cut -d: -f1 || true ;;
	esac
}

is_test_line() { # FILE LINE
	local start
	start=$(test_code_start "$1")
	[[ -n "$start" && "$2" -ge "$start" ]]
}

misnamed_test_modules() { # DIR... -> "file:line: text" per misnamed module
	local dir file
	for dir in "$@"; do
		[[ -d "$dir" ]] || continue
		while IFS= read -r file; do
			awk '
				FNR == 1 { prev = "" }
				prev ~ /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ &&
				$0 ~ /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*;/ {
					name = $0
					sub(/^.*mod[[:space:]]+/, "", name)
					sub(/[[:space:]]*;.*$/, "", name)
					if (name !~ /_tests$/) print FILENAME ":" FNR ": " $0
				}
				{ prev = $0 }' "$file"
		done < <(find "$dir" -name '*.rs')
	done
}
