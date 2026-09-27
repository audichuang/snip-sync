#!/usr/bin/env bash
# scripts/smoke_native.sh
# Thin wrapper around scripts/smoke_native.py for convenient shell invocation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$SCRIPT_DIR/smoke_native.py" "$@"
