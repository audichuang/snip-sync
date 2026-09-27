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
exec xvfb-run -a -s "-screen 0 1280x900x24 -nolisten tcp" "$@"
