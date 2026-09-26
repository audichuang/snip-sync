#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Forward all arguments safely quoted to the Python generator.
# Default preset is 'medium' if not overridden via arguments.
exec python3 "$HERE/scripts/workload_generator.py" "$@"
