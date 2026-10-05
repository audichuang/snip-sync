#!/usr/bin/env bash
# Runs a command in the Linux preflight container: `scripts/linux_container.sh just preflight-linux`.
# `--clean` removes this checkout's containers, volumes, images and kernel cache instead;
# `--prune` only removes the target/ volumes of checkouts that no longer exist;
# `--prune-all` also removes this checkout's (`just clean` uses it).
# Uses Apple's `container` (macOS 26+, Apple silicon): brew install container,
# then container system start --enable-kernel-install.
# The checkout is mounted read-only at its host path, so receipts and fixture paths
# read the same inside and out, and nothing inside writes the host's repository: a git
# refreshing .git/index through the shared mount made the next read see an empty index. The container's target/ is a volume per checkout: Linux
# artifacts never land in the host's target/, and incremental builds survive runs.
# Each volume is labelled with its checkout's path, and every run removes the volumes
# whose checkout is gone: a clone or worktree used once otherwise left 12-18 GB behind.
# SNIP_CONTAINER_CPUS / SNIP_CONTAINER_MEMORY size the VM (container's default is 4 CPUs, 1 GB).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CONTEXT="$ROOT/scripts/linux-container"
KERNEL_CACHE="${XDG_CACHE_HOME:-$HOME/Library/Caches}/snip-sync/kernel"

if ! command -v container >/dev/null 2>&1; then
	echo "linux_container: needs Apple's container CLI: brew install container && container system start --enable-kernel-install" >&2
	exit 1
fi
if ! container system status >/dev/null 2>&1; then
	echo "linux_container: container services are not running: container system start" >&2
	exit 1
fi

hash() { shasum -a 256 | cut -c1-12; }
key="$(printf '%s' "$ROOT" | hash)"
prefix="snip-preflight-$key"
target_volume="snip-preflight-target-$key"

# The CLI exiting does not stop its container: a VM killed client-side keeps running.
# Every container gets a name with this shell's pid, and is killed when the shell exits.
names=()
cleanup() {
	for name in ${names[@]+"${names[@]}"}; do
		container kill "$name" >/dev/null 2>&1 || true
		container rm "$name" >/dev/null 2>&1 || true
	done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Waits in the background so a signal interrupts `wait` instead of waiting for the VM.
run() {
	local name="$prefix-$$-${#names[@]}"
	names+=("$name")
	container run --rm --name "$name" "$@" </dev/null &
	local rc=0
	wait "$!" || rc=$?
	return "$rc"
}

# A shell killed with SIGKILL leaves its containers behind; reap those of dead shells.
while read -r id _; do
	pid="${id#"$prefix"-}"
	pid="${pid%%-*}"
	if [ "$pid" != "$id" ] && ! kill -0 "$pid" 2>/dev/null; then
		container kill "$id" >/dev/null 2>&1 || true
		container rm "$id" >/dev/null 2>&1 || true
	fi
done < <(container ls --all 2>/dev/null | grep "^$prefix-" || true)

# A volume is in use while a container of its checkout exists, and kept while its
# checkout does; one without a label (made before labels) is kept only while in use.
prune_volumes() {
	local containers name root
	containers="$(container ls --all 2>/dev/null | awk 'NR > 1 {print $1}')"
	while read -r name _; do
		case "$name" in snip-preflight-target-*) ;; *) continue ;; esac
		[ "$name" != "$target_volume" ] || continue
		if grep -q "^snip-preflight-${name#snip-preflight-target-}-" <<<"$containers"; then continue; fi
		root="$(container volume inspect "$name" 2>/dev/null | plutil -extract 0.configuration.labels.snip-root raw - 2>/dev/null || true)"
		if [ -n "$root" ] && [ -d "$root" ]; then continue; fi
		if container volume rm "$name" >/dev/null 2>&1; then
			echo "linux_container: removed $name (${root:-unlabelled}, checkout gone)" >&2
		fi
	done < <(container volume ls 2>/dev/null)
}
prune_volumes
if [ "${1:-}" = --prune ]; then
	exit 0
fi
if [ "${1:-}" = --prune-all ]; then
	if container volume rm "$target_volume" >/dev/null 2>&1; then
		echo "linux_container: removed $target_volume ($ROOT)" >&2
	fi
	exit 0
fi

if [ "${1:-}" = --clean ]; then
	container volume rm "$target_volume" snip-preflight-cargo-registry snip-preflight-cargo-git >/dev/null 2>&1 || true
	container image ls | awk '$1 == "snip-preflight" {print $1 ":" $2}' | xargs -n1 container image rm >/dev/null 2>&1 || true
	rm -rf "$KERNEL_CACHE"
	echo "linux_container: removed volumes, snip-preflight images and $KERNEL_CACHE"
	exit 0
fi

cpus="${SNIP_CONTAINER_CPUS:-$(($(sysctl -n hw.ncpu) * 2 / 3))}"
memory="${SNIP_CONTAINER_MEMORY:-$(($(sysctl -n hw.memsize) / 1073741824 / 2))g}"

image="snip-preflight:$(cat "$CONTEXT"/* | hash)"
if ! container image inspect "$image" >/dev/null 2>&1; then
	builder_was_up=0
	container builder status 2>/dev/null | grep -q running && builder_was_up=1
	container build -t "$image" "$CONTEXT"
	# The builder VM stays up after a build; stop it unless something else started it.
	[ "$builder_was_up" = 1 ] || container builder stop >/dev/null 2>&1 || true
	container image ls | awk -v keep="${image#*:}" '$1 == "snip-preflight" && $2 != keep {print $1 ":" $2}' |
		xargs -n1 container image rm >/dev/null 2>&1 || true
fi

# container's default kernel lacks CONFIG_PROC_CHILDREN, and the memory harness fails
# closed without /proc/<pid>/task/<tid>/children. Rebuild that same kernel with it, once
# per default kernel, and boot only these containers with it (-k); the default stays.
kernel_options=(CHECKPOINT_RESTORE PROC_CHILDREN)
props="$(container system property list)"
prop() { printf '%s\n' "$props" | awk -v k="$1" '/^\[/ {s = $0} s == "[kernel]" && $1 == k {gsub(/"/, "", $3); print $3}'; }
base="$HOME/Library/Application Support/com.apple.container/kernels/$(basename "$(prop binaryPath)")"
if [ ! -f "$base" ]; then
	echo "linux_container: default kernel not installed: container system kernel set --recommended" >&2
	exit 1
fi
kernel="$KERNEL_CACHE/vmlinux-$(printf '%s %s' "$(prop digest)" "${kernel_options[*]}" | hash)"
if [ ! -f "$kernel" ]; then
	mkdir -p "$KERNEL_CACHE"
	echo "linux_container: building $(basename "$kernel") (${kernel_options[*]}), a few minutes once" >&2
	# shellcheck disable=SC2016 # expands inside the container, from the arguments after `_`
	run -c "$cpus" -m "$memory" -v "$KERNEL_CACHE:/out" -v "$(dirname "$base"):/base:ro" ubuntu:24.04 bash -euc '
		apt-get update -qq && apt-get install -y -qq --no-install-recommends \
			bc bison build-essential ca-certificates curl flex libelf-dev libssl-dev xz-utils >/dev/null
		ver="$(strings -n 20 "/base/$2" | sed -n "s/^Linux version \([0-9.]*\) .*/\1/p" | head -1)"
		cd /tmp && curl -sSfL "https://cdn.kernel.org/pub/linux/kernel/v${ver%%.*}.x/linux-$ver.tar.xz" | tar -xJ
		cd "linux-$ver" && scripts/extract-ikconfig "/base/$2" > .config
		for o in "${@:3}"; do scripts/config -e "$o"; done
		make -s olddefconfig
		for o in "${@:3}"; do grep -qx "CONFIG_$o=y" .config; done
		make -s -j"$(nproc)" Image
		cp arch/arm64/boot/Image "/out/$1.tmp" && mv "/out/$1.tmp" "/out/$1"' _ "$(basename "$kernel")" "$(basename "$base")" "${kernel_options[@]}"
fi

container volume inspect "$target_volume" >/dev/null 2>&1 ||
	container volume create --label "snip-root=$ROOT" "$target_volume" >/dev/null
for name in snip-preflight-cargo-registry snip-preflight-cargo-git; do
	container volume inspect "$name" >/dev/null 2>&1 || container volume create "$name" >/dev/null
done

# native-acceptance runs five gates at once by default, each with its own Xvfb and
# CPU-rendered apps. CI gives every gate its own runner and runs one collaboration step
# at a time. In one VM, whose CPU quota throttles all of them together, five gates and
# even two missed fixed waits (Xvfb startup, app log lines); one of each, as in CI, holds.
export SNIP_ACCEPTANCE_ARGS="${SNIP_ACCEPTANCE_ARGS---jobs 1 --collaboration-jobs 1}"

# Only settings that mean the same inside; host paths (SNIP_NATIVE_PYTHON, SNIP_NATIVE_BIN) do not.
envs=()
for name in SNIP_E2E_TIMEOUT_SCALE SNIP_REQUIRE_ALL_TESTS SNIP_ACCEPTANCE_ARGS CARGO_BUILD_JOBS; do
	if [ -n "${!name+x}" ]; then envs+=(-e "$name"); fi
done

# The entrypoint copies acceptance evidence here, since the container's /tmp goes with it.
evidence_root="$(mkdir -p "${TMPDIR:-/tmp}/snip-preflight-evidence" && cd "${TMPDIR:-/tmp}/snip-preflight-evidence" && pwd -P)"
evidence="$evidence_root/$(date +%Y%m%d-%H%M%S)-$$"
mkdir "$evidence"
report_evidence() {
	if [ -n "$(ls -A "$evidence")" ]; then
		echo "linux_container: acceptance evidence in $evidence" >&2
	else
		rmdir "$evidence"
	fi
	# Keep the last five runs.
	find "$evidence_root" -mindepth 1 -maxdepth 1 -type d | sort | sed -e :a -e '$d;N;2,5ba' -e 'P;D' | xargs rm -rf
}
trap 'report_evidence; cleanup' EXIT

run --init --shm-size=2g -c "$cpus" -m "$memory" -k "$kernel" \
	-v "$ROOT:$ROOT:ro" -w "$ROOT" -v "$evidence:/evidence" \
	-v "$target_volume:$ROOT/target" \
	-v snip-preflight-cargo-registry:/opt/cargo/registry \
	-v snip-preflight-cargo-git:/opt/cargo/git \
	${envs[@]+"${envs[@]}"} "$image" "$@"
