#!/usr/bin/env bash
# Builds the release Tauri app, then measures it with bench_tauri_memory.py.
# Usage: scripts/measure_tauri.sh --workspace <dataset> --out-dir <new dir> [--runs N] [...]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD=(bun run --cwd "$HERE/crates/desktop" tauri build --no-bundle)

"${BUILD[@]}"
exec python3 -B "$HERE/scripts/bench_tauri_memory.py" \
	--bin "$HERE/target/release/snip-sync" \
	--build-command "${BUILD[*]}" \
	--build-profile release \
	"$@"
