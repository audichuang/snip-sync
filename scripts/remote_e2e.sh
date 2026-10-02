#!/usr/bin/env bash
# Remote-node end to end, CLI only: a real `snip worker` process serves
# folders, a real `snip remote` master pairs with it and reads them over TLS.
# Independent of native acceptance (the GUI gates in the container).
#
#   scripts/remote_e2e.sh --snip target/debug/snip
#       Worker on this machine at 127.0.0.1 (CI and `just preflight`).
#   scripts/remote_e2e.sh --snip target/release/snip --worker-ssh ubuntu \
#           [--listen 100.x.y.z:47821] [--remote-snip /path/to/snip]
#       Worker on another machine over ssh, reached on its Tailscale address
#       (`tailscale ip -4` there unless --listen). Without --remote-snip the
#       worker is built there from `git archive HEAD`.
#
# Every check prints PASS or FAIL; the exit code is 0 only when all passed.
# Waits scale with SNIP_E2E_TIMEOUT_SCALE. A check that cannot run here
# (no symlinks) fails instead of skipping when SNIP_REQUIRE_ALL_TESTS is set.
set -uo pipefail

SNIP=""
HOST=""
LISTEN=""
RSNIP=""
while [ $# -gt 0 ]; do
	case "$1" in
	--snip) SNIP=$2; shift 2 ;;
	--worker-ssh) HOST=$2; shift 2 ;;
	--listen) LISTEN=$2; shift 2 ;;
	--remote-snip) RSNIP=$2; shift 2 ;;
	-h | --help) sed -n '2,17p' "$0"; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
[ -n "$SNIP" ] || { echo "--snip <path to the master's snip> is required" >&2; exit 2; }
[ -x "$SNIP" ] || { echo "$SNIP is not an executable" >&2; exit 2; }
SNIP=$(cd "$(dirname "$SNIP")" && pwd)/$(basename "$SNIP")
REPO=$(cd "$(dirname "$0")/.." && pwd)
SCALE=${SNIP_E2E_TIMEOUT_SCALE:-1}

# Runs a bash snippet on the worker machine (stdin), prints its output.
w() {
	if [ -z "$HOST" ]; then
		bash -s
	else
		ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 "$HOST" bash -s
	fi
}

hash_of() { # stdin -> sha256 hex
	if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -c1-64
}

pass=0
fail=0
ok() { pass=$((pass + 1)); echo "PASS  $*"; }
bad() { fail=$((fail + 1)); echo "FAIL  $*"; }
check() { # name, command...
	local name=$1
	shift
	if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}
skip() {
	if [ -n "${SNIP_REQUIRE_ALL_TESTS:-}" ]; then
		bad "$1 (cannot run here: $2; SNIP_REQUIRE_ALL_TESTS is set)"
	else
		echo "SKIP  $1 ($2)"
	fi
}
refused() { # name, expected stderr fragment ("" = any), snip remote args...
	local name=$1 want=$2 err rc
	shift 2
	err=$("$SNIP" remote "$@" 2>&1 >/dev/null)
	rc=$?
	if [ "$rc" = 1 ] && [[ "$err" == *"$want"* ]]; then
		ok "$name  ($err)"
	else
		bad "$name  rc=$rc err=$err"
	fi
}

MASTER=$(mktemp -d)
export SNIP_CONFIG_DIR="$MASTER/master" # a fresh master: pairing is part of the run
WD=$(w <<<'mktemp -d')
[ -n "$WD" ] || { echo "cannot make a work folder on the worker" >&2; exit 1; }

cleanup() {
	w <<<"[ -f '$WD/worker.pid' ] && kill \$(cat '$WD/worker.pid') 2>/dev/null; sleep 1; rm -rf '$WD'" >/dev/null 2>&1
	rm -rf "$MASTER"
}
trap cleanup EXIT

echo "== worker machine: ${HOST:-this machine}, work folder $WD"
SRC=""
if [ -z "$HOST" ]; then
	RSNIP=${RSNIP:-$SNIP}
	SRC=$REPO
elif [ -z "$RSNIP" ]; then
	echo "== building the worker there from git archive HEAD ($(git -C "$REPO" rev-parse --short HEAD))"
	git -C "$REPO" archive HEAD | ssh -o BatchMode=yes "$HOST" "mkdir -p '$WD/src' && tar -x -C '$WD/src'" ||
		{ echo "cannot copy the source to $HOST" >&2; exit 1; }
	w <<<"cd '$WD/src' && export PATH=\$HOME/.cargo/bin:\$PATH && cargo build --release -p snip-cli --locked 2>&1 | tail -1" ||
		{ echo "the worker build failed on $HOST" >&2; exit 1; }
	RSNIP="$WD/src/target/release/snip"
	SRC="$WD/src"
fi
if [ -z "$LISTEN" ]; then
	if [ -z "$HOST" ]; then
		LISTEN=127.0.0.1:0
	else
		ip=$(w <<<'tailscale ip -4 2>/dev/null | head -1')
		[ -n "$ip" ] || { echo "no Tailscale address on $HOST; pass --listen" >&2; exit 2; }
		LISTEN=$ip:47821
	fi
fi

# Fixtures, made on the worker. `secret.txt` sits outside every share.
w <<EOF || { echo "cannot make the fixtures" >&2; exit 1; }
set -e
cd '$WD'
export MSYS=winsymlinks:nativestrict # Git Bash: real symlinks or an error
mkdir -p edge/src/deep edge/nested/.git edge/manydir text
echo top-secret > secret.txt
printf 'line1\r\nline2\r\n' > edge/crlf.txt
: > edge/empty.txt
echo '深層 檔案' > 'edge/src/deep/中文 有空白.txt'
printf '\000\001\002binary' > edge/blob.bin
printf '\377\376' > edge/latin.txt
head -c 1048576 /dev/zero | tr '\000' a > edge/exact-1MiB.txt
head -c 1048577 /dev/zero | tr '\000' a > edge/over-1MiB.txt
(cd edge/manydir && touch f{1..1200})
ln -s "\$PWD/secret.txt" edge/escape.txt 2>/dev/null || true
ln -s "\$PWD/edge/src" edge/inner-link 2>/dev/null || true
# Text the transport must carry byte for byte.
for i in \$(seq 1 60); do
	printf 'file %d\tTab 中文 ✓ 🦀 é\n%s\n' "\$i" "\$(head -c \$((i * 97)) /dev/zero | tr '\000' x)" > "text/plain_\$i.txt"
	printf 'crlf %d\r\nsecond\r\n' "\$i" > "text/crlf_\$i.txt"
	printf 'no newline at the end %d' "\$i" > "text/noeol_\$i.txt"
done
printf '\357\273\277BOM first\n' > text/bom.txt
head -c 200000 /dev/zero | tr '\000' 'y' > text/long-line.txt
EOF

start_worker() { # config folder name; prints the worker log
	local tries=0 log=""
	while [ $tries -lt 10 ]; do
		w <<EOF
cd '$WD'
[ -f worker.pid ] && kill \$(cat worker.pid) 2>/dev/null && sleep 1
shares="--share '$WD/edge' --share '$WD/text'"
[ -n '$SRC' ] && shares="\$shares --share '$SRC'"
eval "SNIP_CONFIG_DIR='$WD/$1' nohup '$RSNIP' worker \$shares --listen '$LISTEN' > worker.log 2>&1 &"
echo \$! > worker.pid
EOF
		local waited=0 limit=$((20 * SCALE))
		while [ $waited -lt "$limit" ]; do
			log=$(w <<<"cat '$WD/worker.log' 2>/dev/null")
			case "$log" in
			*"pairing code"*) echo "$log"; return 0 ;;
			*rror*) break ;;
			esac
			sleep 1
			waited=$((waited + 1))
		done
		# A port just released can still be held for a moment (Windows).
		tries=$((tries + 1))
		sleep 1
	done
	echo "$log"
	return 1
}

echo "== start the worker"
log=$(start_worker wcfg) || { echo "$log"; bad "worker starts"; exit 1; }
echo "$log"
ADDR=$(sed -n 's/^snip-sync worker listening on //p' <<<"$log" | tr -d '\r')
LISTEN=$ADDR # restarts reuse the same address
code=$(sed -n 's/^pairing code \([^ ]*\).*/\1/p' <<<"$log")
wfp=$(sed -n 's/^fingerprint //p' <<<"$log" | tr -d '\r')

echo "== pairing"
mkdir -p "$SNIP_CONFIG_DIR"
decoy_fp=$(printf 'ab%.0s' {1..32})
printf '[{"name":"decoy","addr":"192.0.2.1:47821","fingerprint":"%s"}]\n' "$decoy_fp" > "$SNIP_CONFIG_DIR/remote-workers.json"
out=$("$SNIP" remote pair "$ADDR" "$code" 2>&1)
check "pair with the printed code" grep -q "^paired with" <<<"$out"
check "the master shows the worker's fingerprint ($wfp)" grep -q "$wfp" <<<"$out"
w_out=$("$SNIP" remote workers)
check "pairing keeps another worker's entry" test "$(grep -c . <<<"$w_out")" = 2 -a "$(grep -c decoy <<<"$w_out")" = 1 -a "$(head -1 <<<"$w_out" | grep -c "$ADDR")" = 1
"$SNIP" remote forget decoy >/dev/null
w_after=$("$SNIP" remote workers)
check "forgetting another pairing leaves ours" test "$(grep -c . <<<"$w_after")" = 1 -a "$(head -1 <<<"$w_after" | grep -c "^1	")" = 1
check "listed as worker 1" bash -c "'$SNIP' remote workers | grep -q '^1	'"
spaces=$("$SNIP" remote workspaces 1)
expect=2
[ -n "$SRC" ] && expect=3
check "$expect shared workspaces" test "$(grep -c . <<<"$spaces")" = "$expect"
srcname=$(basename "$SRC")

echo "== browsing"
check "folders first" bash -c "'$SNIP' remote ls 1 edge | head -1 | grep -q '/\$'"
check "a Chinese name with spaces" bash -c "'$SNIP' remote cat 1 edge 'src/deep/中文 有空白.txt' | grep -q 深層"
check "stat of an empty file" bash -c "'$SNIP' remote stat 1 edge empty.txt | grep -q '^file	0	'"
check "exactly 1 MiB is served" test "$("$SNIP" remote cat 1 edge exact-1MiB.txt | wc -c | tr -d ' ')" = 1048576
check "1200 entries are cut at 1000" test "$("$SNIP" remote ls 1 edge manydir 2>/dev/null | wc -l | tr -d ' ')" = 1000
check "a nested repo is a folder" bash -c "'$SNIP' remote stat 1 edge nested | grep -q '^directory'"
if w <<<"[ -L '$WD/edge/inner-link' ]"; then
	check "a symlink inside the share is followed" bash -c "'$SNIP' remote ls 1 edge inner-link | grep -qx deep/"
else
	skip "a symlink inside the share is followed" "no symlinks on the worker"
fi

echo "== byte for byte"
compare() { # workspace, folder on the worker, find arguments as shell text
	local ws=$1 dir=$2 expr=$3 sums total=0 badfiles=0
	sums=$(w <<EOF
cd '$dir' && find $expr -type f | LC_ALL=C sort | while IFS= read -r f; do
	h=\$( (command -v sha256sum >/dev/null && sha256sum < "\$f" || shasum -a 256 < "\$f") | cut -c1-64)
	printf '%s\t%s\n' "\$h" "\$f"
done
EOF
)
	while IFS=$'\t' read -r sum f; do
		[ -n "$f" ] || continue
		total=$((total + 1))
		f=${f#./}
		if [ "$("$SNIP" remote cat 1 "$ws" "$f" | hash_of)" != "$sum" ]; then
			badfiles=$((badfiles + 1))
			echo "      differs: $f"
		fi
	done <<<"$sums"
	check "$total files in $ws identical to the worker's bytes" test "$badfiles" = 0 -a "$total" -gt 0
}
compare text "$WD/text" .
check "CRLF kept" test "$("$SNIP" remote cat 1 edge crlf.txt | hash_of)" = "$(printf 'line1\r\nline2\r\n' | hash_of)"
if [ -n "$SRC" ]; then
	compare "$srcname" "$SRC" "crates docs scripts fixtures -size -1024k \\( -name '*.rs' -o -name '*.md' -o -name '*.toml' -o -name '*.py' -o -name '*.sh' -o -name '*.json' \\)"
fi

echo "== refused"
refused "binary" "binary or not UTF-8" cat 1 edge blob.bin
refused "not UTF-8" "binary or not UTF-8" cat 1 edge latin.txt
refused "over 1 MiB" "exceeds 1 MiB" cat 1 edge over-1MiB.txt
refused "../" "inside the workspace" cat 1 edge ../secret.txt
refused "src/../../" "inside the workspace" cat 1 edge src/../../secret.txt
refused "an absolute path" "inside the workspace" cat 1 edge "$WD/secret.txt"
refused "ls .." "inside the workspace" ls 1 edge ..
refused "a folder read as a file" "" cat 1 edge src
refused "an unknown workspace" "no workspace named" ls 1 nope
refused "an unknown worker" "no paired worker" ls nobody edge
if w <<<"[ -L '$WD/edge/escape.txt' ]"; then
	refused "a symlink out of the share" "leaves the workspace" cat 1 edge escape.txt
else
	skip "a symlink out of the share" "no symlinks on the worker"
fi

echo "== parallel"
want=$("$SNIP" remote cat 1 text plain_60.txt | hash_of)
for n in 20 50 100; do
	rm -f "$MASTER"/par.*
	for i in $(seq 1 "$n"); do
		("$SNIP" remote cat 1 text plain_60.txt 2>/dev/null | hash_of >"$MASTER/par.$i") &
	done
	wait
	good=$(cat "$MASTER"/par.* | grep -c "$want")
	check "$n parallel reads all correct ($good/$n)" test "$good" = "$n"
done

echo "== other devices"
other="$MASTER/other"
mkdir -p "$other"
cp "$SNIP_CONFIG_DIR/remote-workers.json" "$other/"
err=$(SNIP_CONFIG_DIR=$other "$SNIP" remote workspaces 1 2>&1)
check "a copied pairing record lets no other device in" grep -q "no longer trusts" <<<"$err"
err=$(SNIP_CONFIG_DIR=$other "$SNIP" remote pair "$ADDR" "$code" 2>&1)
check "a used code pairs nobody else" grep -q "pairing failed" <<<"$err"

echo "== restart"
log=$(start_worker wcfg) || bad "the worker restarts"
check "same fingerprint after a restart" grep -q "fingerprint $wfp" <<<"$log"
check "still paired after a restart" bash -c "'$SNIP' remote cat 1 edge crlf.txt | grep -q line1"

echo "== another certificate at the same address"
start_worker wcfg-other >/dev/null || bad "the other worker starts"
err=$("$SNIP" remote ls 1 edge 2>&1)
check "the pinned master refuses it" grep -q "is not the paired" <<<"$err"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
