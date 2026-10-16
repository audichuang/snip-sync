# shellcheck shell=bash
# Which lines of a Rust file are test code, for every lint that holds product
# code to a rule tests may break (check_test_refresh.sh, check_msg_raw.sh).
# One rule, so the lints cannot disagree. The unit is the block, never "from
# some line to the end of the file": a `#[cfg(test)] mod x {` can be followed
# by product code (desktop main.rs has a `test_log` module near the top). Test
# code is
#   - a file named `*_tests.rs`, whole;
#   - every top-level block `#[cfg(test)]` + `(pub(..) )?mod NAME {`, from the
#     `mod` line to the top-level `}` that closes it, whatever NAME is
#     (`tests`, `row_width_tests`, `test_log`). Other attributes may sit
#     between `#[cfg(test)]` and `mod`; a `mod x { }` on one line is that line.
# A file-level test module named otherwise would read as product code, so
# misnamed_test_modules reports `#[cfg(test)] mod x;` whose x is not `*_tests`.
# Limits: the block must start at column 0 and end at a `}` at column 0 (what
# rustfmt writes; a `}` at column 0 inside a multi-line string literal would
# end it early); a test item that is not a module (`#[cfg(test)] fn helper`) is
# product code to the lints; and misnamed_test_modules sees only `#[cfg(test)]`
# on the line right above `mod x;`, so write out-of-line test modules in the
# plain two-line form.

test_code_ranges() { # FILE -> "first last" per test block, one per line
	awk '
		function flush() { if (start) print start, FNR; start = 0 }
		start {
			if ($0 ~ /^}/) flush()
			next
		}
		/^#\[cfg\(test\)\][[:space:]]*$/ { pending = 1; next }
		pending && /^#\[/ { next }
		pending && /^(pub([(][^)]*[)])?[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*[{]/ {
			pending = 0
			if ($0 ~ /[}][[:space:]]*$/) print FNR, FNR
			else start = FNR
			next
		}
		{ pending = 0 }
		END { if (start) print start, 2147483647 }' "$1"
}

_test_code_file=""
_test_code_cache=""

is_test_line() { # FILE LINE
	local first last
	[[ "$1" == *_tests.rs ]] && return 0
	if [[ "$_test_code_file" != "$1" ]]; then
		_test_code_file="$1"
		_test_code_cache=$(test_code_ranges "$1")
	fi
	while read -r first last; do
		[[ -n "$first" && "$2" -ge "$first" && "$2" -le "$last" ]] && return 0
	done <<<"$_test_code_cache"
	return 1
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

# The line before line $2 of file $1, or nothing on line 1. GNU sed rejects
# line address 0, which BSD sed reads as no line.
line_before() {
	local n=$(($2 - 1))
	((n >= 1)) || return 0
	sed -n "${n}p" "$1"
}
