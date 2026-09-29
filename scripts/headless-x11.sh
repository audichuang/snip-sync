#!/usr/bin/env bash
# Runs a command on a private Xvfb with Mesa's lavapipe (software Vulkan) driver selected
# explicitly. Xvfb has no DRI3, so GPUI's default Vulkan device fails there with
# PlatformNotSupported; lavapipe renders on the CPU instead.
# Usage: scripts/headless-x11.sh <command> [args...]
set -euo pipefail

icd=""
# The file name differs by Mesa build: lvp_icd.json (newer), lvp_icd.<arch>.json (Ubuntu 24.04),
# or multiarch directories under /usr/share or /etc.
for candidate in \
	"/usr/share/vulkan/icd.d/lvp_icd.json" \
	"/usr/share/vulkan/icd.d/lvp_icd.$(uname -m).json" \
	/usr/share/vulkan/icd.d/lvp_icd.*.json \
	/etc/vulkan/icd.d/lvp_icd.json \
	/etc/vulkan/icd.d/lvp_icd.*.json; do
	if [ -f "$candidate" ]; then
		icd="$candidate"
		break
	fi
done
if [ -z "$icd" ]; then
	echo "headless-x11: no lavapipe ICD under /usr/share/vulkan/icd.d or /etc/vulkan/icd.d (install mesa-vulkan-drivers)" >&2
	exit 1
fi

export VK_DRIVER_FILES="$icd"
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=llvmpipe
echo "headless-x11: VK_DRIVER_FILES=$icd" >&2

# `xvfb-run -a` picks a display number by looking for a free lock file and then starts Xvfb on it,
# so two of them started together can pick the same number and end up sharing one server (a shared
# clipboard, the wrong focus). Xvfb's own -displayfd picks the number atomically, so parallel runs
# each get a private display.
tmp="$(mktemp -d)"
xvfb=""
child=""
cleanup() {
	[ -z "$child" ] || kill "$child" 2>/dev/null || true
	[ -z "$xvfb" ] || kill "$xvfb" 2>/dev/null || true
	rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 143' TERM INT
Xvfb -displayfd 3 -screen 0 1280x900x24 -nolisten tcp 3>"$tmp/display" 2>"$tmp/xvfb.log" &
xvfb=$!
for _ in $(seq 1 600); do
	[ -s "$tmp/display" ] && break
	kill -0 "$xvfb" 2>/dev/null || { echo "headless-x11: Xvfb exited before reporting a display" >&2; cat "$tmp/xvfb.log" >&2; exit 1; }
	sleep 0.1
done
[ -s "$tmp/display" ] || { echo "headless-x11: Xvfb reported no display within 60 s" >&2; exit 1; }
display="$(tr -d '\n' <"$tmp/display")"
export DISPLAY=":$display"
# In the background so a TERM reaches the trap at once instead of after the command ends.
"$@" &
child=$!
wait "$child"
