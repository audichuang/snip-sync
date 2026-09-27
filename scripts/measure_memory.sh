#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Forward all arguments safely quoted to the Python memory harness.
exec python3 "$HERE/scripts/memory_harness.py" "$@"
