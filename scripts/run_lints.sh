#!/usr/bin/env bash
set -uo pipefail

# Runs every scripts/check_*.sh, in name order, and fails if any of them does.
# The justfile and CI call this one script, so a new lint is a new file here
# and no other change: it cannot be written and left out of a gate (#230).
#
# Usage: run_lints.sh [ROOT]   (ROOT is passed on; it defaults to the repository root)

DIR="$(cd "$(dirname "$0")" && pwd)"
failed=()
ran=0

for lint in "$DIR"/check_*.sh; do
	[[ -f "$lint" ]] || continue
	name="$(basename "$lint")"
	ran=$((ran + 1))
	echo "== $name"
	if ! bash "$lint" "$@"; then
		failed+=("$name")
	fi
done

if [ "$ran" -eq 0 ]; then
	echo "ERROR: no scripts/check_*.sh found in $DIR" >&2
	exit 1
fi

if [ "${#failed[@]}" -ne 0 ]; then
	echo "FAILED: ${failed[*]}" >&2
	exit 1
fi

echo "run_lints: $ran lint(s) passed."
