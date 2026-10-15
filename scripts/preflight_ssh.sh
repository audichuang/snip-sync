#!/usr/bin/env bash
# Runs CI's Linux jobs (`just preflight-linux-checks`, then `just preflight-acceptance`) in
# Docker on another Linux machine: `scripts/preflight_ssh.sh <host>`.
# `scripts/preflight_ssh.sh --clean <host>` removes everything it left on the host.
# `just preflight-ssh <host>` runs this beside `just preflight-host` here.
# The host needs docker (the ssh user in the docker group), git, rsync and flock; no
# special kernel, unlike linux_container.sh. The code travels as a git push of HEAD into one
# persistent repo on the host, the gitignored .ts-ref/ by rsync: only deltas cross. That
# repo's path is fixed, so its target/ volume survives a pass and the next run is incremental.
# The tree must be committed: native-acceptance fails if the checkout changes. A git
# worktree works, since only the commit is sent.
# SNIP_PREFLIGHT_SSH_DIR (default ~/.cache/snip-preflight/repo), _CPUS (default 3/4 of the
# host's), _MEMORY (16g), _LOCK_WAIT (seconds to wait for another run, 1800),
# _TARGET_MAX_GB (40: a bigger target volume is dropped after a run).
set -euo pipefail

# Settings sent to the host; the ones that mean the same there.
forward=(SNIP_E2E_TIMEOUT_SCALE SNIP_REQUIRE_ALL_TESTS SNIP_ACCEPTANCE_ARGS CARGO_BUILD_JOBS
	SNIP_PREFLIGHT_SSH_CPUS SNIP_PREFLIGHT_SSH_MEMORY SNIP_PREFLIGHT_SSH_TARGET_MAX_GB)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -c1-12; }

if [ "${1:-}" = --remote ]; then
	# On the host, started by the bootstrap below with the lock held (fd 9) and the
	# checkout at the pushed sha.
	run="$2"
	CONTEXT="$ROOT/scripts/linux-container"
	base="$(dirname "$ROOT")"
	name="snip-preflight-ssh-$run"
	evidence_root="$base/evidence"
	evidence="$evidence_root/$run"
	mkdir -p "$evidence" "$ROOT/target"

	# The ssh session ending does not stop its container; kill it by name on any exit.
	# A closed session shows as SIGPIPE on the next write.
	# shellcheck disable=SC2317,SC2329 # runs as the EXIT trap (0.9 says SC2317, newer SC2329)
	finish() {
		local rc=$?
		trap '' EXIT INT TERM HUP PIPE
		set +e
		docker kill "$name" >/dev/null 2>&1 || true
		if [ -n "$(ls -A "$evidence")" ]; then
			# The entrypoint copied it as root; hand it back so it can be pruned and fetched.
			docker run --rm --entrypoint chown -v "$evidence:/e" "$image" -R 1000:1000 /e >/dev/null 2>&1 || true
			echo "preflight-ssh: acceptance evidence in $evidence" >&2
		else
			rmdir "$evidence"
		fi
		# Keep the last three runs.
		find "$evidence_root" -mindepth 1 -maxdepth 1 -type d | sort | sed -e :a -e '$d;N;2,3ba' -e 'P;D' | xargs -r rm -rf
		# The target volume is kept for the next run's incremental build, up to a cap.
		bytes="$(docker run --rm --entrypoint du -v snip-preflight-ssh-target:/v "$image" -sb /v 2>/dev/null | cut -f1)"
		if [ "${bytes:-0}" -gt $((${SNIP_PREFLIGHT_SSH_TARGET_MAX_GB:-40} * 1000000000)) ]; then
			docker volume rm snip-preflight-ssh-target >/dev/null 2>&1 &&
				echo "preflight-ssh: dropped the target volume ($((bytes / 1000000000)) GB)" >&2
		fi
		exit "$rc"
	}
	image="snip-preflight:$(cat "$CONTEXT"/* | hash)"
	trap finish EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM
	trap 'exit 129' HUP
	trap 'exit 141' PIPE

	# The lock means no other run is alive: whatever is left was orphaned by a dead session.
	docker ps -aq --filter name=snip-preflight-ssh- | xargs -r docker rm -f >/dev/null 2>&1 || true

	if ! docker image inspect "$image" >/dev/null 2>&1; then
		docker build -t "$image" "$CONTEXT"
		docker images --format '{{.Repository}}:{{.Tag}}' snip-preflight | grep -vx "$image" |
			xargs -r docker rmi >/dev/null 2>&1 || true
	fi

	cpus="${SNIP_PREFLIGHT_SSH_CPUS:-$(($(nproc) * 3 / 4))}"
	memory="${SNIP_PREFLIGHT_SSH_MEMORY:-16g}"
	# As in linux_container.sh: acceptance with five gates at once misses fixed waits.
	export SNIP_ACCEPTANCE_ARGS="${SNIP_ACCEPTANCE_ARGS---jobs 2 --collaboration-jobs 1}"
	envs=()
	for var in SNIP_E2E_TIMEOUT_SCALE SNIP_REQUIRE_ALL_TESTS SNIP_ACCEPTANCE_ARGS CARGO_BUILD_JOBS; do
		if [ -n "${!var+x}" ]; then envs+=(-e "$var"); fi
	done

	# Waits in the background so a signal interrupts `wait` instead of waiting for docker.
	# --memory-swap equal to --memory: no swap, so a runaway build is killed, not the host.
	docker run --rm --init --shm-size=2g --name "$name" --cpus "$cpus" --memory "$memory" --memory-swap "$memory" \
		-v "$ROOT:$ROOT:ro" -w "$ROOT" -v "$evidence:/evidence" \
		-v "snip-preflight-ssh-target:$ROOT/target" \
		-v snip-preflight-ssh-cargo-registry:/opt/cargo/registry \
		-v snip-preflight-ssh-cargo-git:/opt/cargo/git \
		${envs[@]+"${envs[@]}"} "$image" \
		bash -c 'just preflight-linux-checks && just preflight-acceptance' </dev/null &
	rc=0
	wait "$!" || rc=$?
	exit "$rc"
fi

ssh_opts=(-o BatchMode=yes -o ServerAliveInterval=30 -o ConnectTimeout=15)

# Sent to the host: resolves the repo directory (~ expanded there) and makes the repo once.
read -r -d '' prepare <<'EOF' || true
set -eu
d=$1
case $d in "~/"*) d=$HOME/${d#"~/"} ;; esac
mkdir -p "$d"
[ -d "$d/.git" ] || git -C "$d" init -q
printf '%s\n' "$d"
EOF

if [ "${1:-}" = --clean ]; then
	host="${2:?usage: preflight_ssh.sh --clean <host>}"
	rdir="$(ssh "${ssh_opts[@]}" "$host" bash -s -- "${SNIP_PREFLIGHT_SSH_DIR:-~/.cache/snip-preflight/repo}" <<<"$prepare")"
	ssh "${ssh_opts[@]}" "$host" bash -s -- "$rdir" <<'EOF'
set -u
base=$(dirname "$1")
exec 9>>"$base/lock"
flock -n 9 || { echo "preflight-ssh: a run holds $base/lock: $(cat "$base/lock")" >&2; exit 1; }
docker ps -aq --filter name=snip-preflight-ssh- | xargs -r docker rm -f >/dev/null 2>&1
docker volume rm snip-preflight-ssh-target snip-preflight-ssh-cargo-registry snip-preflight-ssh-cargo-git >/dev/null 2>&1
docker images -q snip-preflight | sort -u | xargs -r docker rmi -f >/dev/null 2>&1
rm -rf "$1" "$base/evidence" "$base/ts-ref"
echo "preflight-ssh: removed the volumes, snip-preflight images and $base contents"
EOF
	exit 0
fi

host="${1:?usage: preflight_ssh.sh <host> | --clean <host>}"
cd "$ROOT"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
	echo "preflight-ssh: the tree has uncommitted changes; commit first (only the commit is sent)" >&2
	exit 1
fi
sha="$(git rev-parse HEAD)"

# .ts-ref/ is gitignored and lives in the main checkout, not in a worktree.
tsref="$ROOT/.ts-ref"
if [ ! -d "$tsref" ]; then
	main="$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd -P)"
	tsref="$main/.ts-ref"
fi
if [ ! -d "$tsref" ]; then
	if [ -n "${SNIP_REQUIRE_ALL_TESTS:-}" ]; then
		echo "preflight-ssh: no .ts-ref/ (see docs/plan.md section 2); the cross-tool tests need it" >&2
		exit 1
	fi
	echo "preflight-ssh: no .ts-ref/: the cross-tool tests will skip" >&2
	tsref=
fi

rdir="$(ssh "${ssh_opts[@]}" "$host" bash -s -- "${SNIP_PREFLIGHT_SSH_DIR:-~/.cache/snip-preflight/repo}" <<<"$prepare")"
rbase="$(dirname "$rdir")"
run="$(date +%Y%m%d-%H%M%S)-$$"
name="snip-preflight-ssh-$run"

# Killing the ssh client does not stop the host's container, so kill it there as well.
finished=0
# shellcheck disable=SC2317,SC2329 # runs as the EXIT trap (0.9 says SC2317, newer SC2329)
cleanup() {
	if [ "$finished" = 0 ]; then
		# shellcheck disable=SC2029 # expands here on purpose
		ssh "${ssh_opts[@]}" "$host" docker kill "$name" >/dev/null 2>&1 || true
	fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

echo "preflight-ssh: sending $sha to $host:$rdir" >&2
GIT_SSH_COMMAND="ssh ${ssh_opts[*]}" git push -q --force "$host:$rdir" "$sha:refs/preflight/$sha"
if [ -n "$tsref" ]; then
	# Staged beside the repo and copied in under the lock, so a run in progress is not touched.
	rsync -a --delete --checksum -e "ssh ${ssh_opts[*]}" "$tsref/" "$host:$rbase/ts-ref/"
fi

# The env (only what is set) goes first, then the bootstrap: waits for the lock, checks out
# the sha, and hands over to this script's --remote half from that checkout.
bootstrap() {
	for var in "${forward[@]}"; do
		if [ -n "${!var+x}" ]; then printf 'export %s=%q\n' "$var" "${!var}"; fi
	done
	cat <<'EOF'
set -eu
d=$1 sha=$2 run=$3 wait=$4
base=$(dirname "$d")
exec 9>>"$base/lock"
if ! flock -w "$wait" 9; then
	echo "preflight-ssh: $base/lock still held after ${wait}s by: $(cat "$base/lock")" >&2
	exit 1
fi
printf '%s from %s since %s\n' "$run" "${SSH_CLIENT%% *}" "$(date +%H:%M:%S)" >"$base/lock"
git -C "$d" checkout -q -f --detach "$sha"
git -C "$d" reset -q --hard "$sha"
git -C "$d" clean -ffdxq -e /target -e /.ts-ref
git -C "$d" for-each-ref --format='%(refname)' refs/preflight | grep -vx "refs/preflight/$sha" |
	xargs -r -n1 git -C "$d" update-ref -d || true
if [ -d "$base/ts-ref" ]; then
	mkdir -p "$d/.ts-ref"
	rsync -a --delete "$base/ts-ref/" "$d/.ts-ref/"
else
	rm -rf "$d/.ts-ref"
fi
exec "$d/scripts/preflight_ssh.sh" --remote "$run" </dev/null
EOF
}
rc=0
ssh "${ssh_opts[@]}" "$host" bash -s -- "$rdir" "$sha" "$run" "${SNIP_PREFLIGHT_SSH_LOCK_WAIT:-1800}" < <(bootstrap) &
wait "$!" || rc=$?
finished=1

if [ "$rc" -eq 0 ]; then
	echo "preflight-ssh: passed; evidence, if any, in $host:$rbase/evidence/$run" >&2
	exit 0
fi
# Evidence only exists once the container ran; copy it here for a failure.
local_root="${TMPDIR:-/tmp}/snip-preflight-evidence"
mkdir -p "$local_root"
if rsync -a -e "ssh ${ssh_opts[*]}" "$host:$rbase/evidence/$run/" "$local_root/$run/" 2>/dev/null; then
	echo "preflight-ssh: failed (exit $rc); acceptance evidence in $local_root/$run" >&2
else
	echo "preflight-ssh: failed (exit $rc); no evidence copied" >&2
fi
exit "$rc"
