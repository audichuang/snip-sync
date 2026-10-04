#!/usr/bin/env bash
# Remote workspaces end to end, CLI only: a real `snip remote` master starts
# a real `snip serve --stdio` worker for every command and reads folders
# through it. Independent of native acceptance (the GUI gates in the
# container).
#
#   scripts/remote_e2e.sh --snip target/debug/snip
#       Worker on this machine, started directly (CI and `just preflight`).
#   scripts/remote_e2e.sh --snip target/release/snip --worker-ssh ubuntu \
#           [--remote-snip /path/to/snip]
#       Worker on another machine, started over ssh as a master does.
#       Without --remote-snip the worker is built there from
#       `git archive HEAD`.
#
# Every check prints PASS or FAIL; the exit code is 0 only when all passed.
# Waits scale with SNIP_E2E_TIMEOUT_SCALE. A check that cannot run here
# (no symlinks) fails instead of skipping when SNIP_REQUIRE_ALL_TESTS is set.
set -uo pipefail

SNIP=""
HOST=""
RSNIP=""
while [ $# -gt 0 ]; do
	case "$1" in
	--snip) SNIP=$2; shift 2 ;;
	--worker-ssh) HOST=$2; shift 2 ;;
	--remote-snip) RSNIP=$2; shift 2 ;;
	-h | --help) sed -n '2,18p' "$0"; exit 0 ;;
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
export SNIP_CONFIG_DIR="$MASTER/master"
WD=$(w <<<'mktemp -d')
[ -n "$WD" ] || { echo "cannot make a work folder on the worker" >&2; exit 1; }

cleanup() {
	w <<<"rm -rf '$WD'" >/dev/null 2>&1
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
# How every `snip remote` below starts its worker: directly, or over ssh as
# a master does. The host argument `h` is then only a label.
if [ -z "$HOST" ]; then
	export SNIP_REMOTE_EXEC="$RSNIP serve --stdio"
else
	export SNIP_REMOTE_EXEC="ssh -T -o BatchMode=yes -o ConnectTimeout=10 $HOST $RSNIP serve --stdio"
fi

# Fixtures, made on the worker. `secret.txt` sits outside every workspace.
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
ln -s "\$PWD" edge/escape-dir 2>/dev/null || true
# Text the transport must carry byte for byte.
for i in \$(seq 1 60); do
	printf 'file %d\tTab 中文 ✓ 🦀 é\n%s\n' "\$i" "\$(head -c \$((i * 97)) /dev/zero | tr '\000' x)" > "text/plain_\$i.txt"
	printf 'crlf %d\r\nsecond\r\n' "\$i" > "text/crlf_\$i.txt"
	printf 'no newline at the end %d' "\$i" > "text/noeol_\$i.txt"
done
printf '\357\273\277BOM first\n' > text/bom.txt
head -c 200000 /dev/zero | tr '\000' 'y' > text/long-line.txt
# Git fixtures for git views
echo TOPSECRET > git-secret.txt
G="git -c user.name=t -c user.email=t@t"
g_init() {
	\$G init -b main "\$1" 2>/dev/null || { \$G init "\$1" && (cd "\$1" && \$G checkout -B main 2>/dev/null || true); }
}
mkdir -p gitws/plain gitws/broken/.git outer/inner
echo plain > gitws/plain/file.txt

g_init gitws/alpha
printf 'commit 1 a\n' > gitws/alpha/a.txt
(cd gitws/alpha && \$G add a.txt && \$G commit -q -m "first commit")
printf 'commit 2 a\n' > gitws/alpha/a.txt
printf 'commit 2 b\n' > gitws/alpha/b.txt
(cd gitws/alpha && \$G add a.txt b.txt && \$G commit -q -m "second commit")
printf 'commit 2 a modified\n' > gitws/alpha/a.txt
printf 'new file content\n' > gitws/alpha/new.txt
printf 'staged file content\n' > gitws/alpha/staged.txt
(cd gitws/alpha && \$G add staged.txt)
ln -s "\$PWD/git-secret.txt" gitws/alpha/link-to-secret 2>/dev/null || true

g_init gitws/beta
printf 'beta content\n' > gitws/beta/b.txt
(cd gitws/beta && \$G add b.txt && \$G commit -q -m "beta commit")

g_init outside-repo
printf 'outside\n' > outside-repo/outside.txt
(cd outside-repo && \$G add outside.txt && \$G commit -q -m "outside commit")
(cd outside-repo && \$G worktree add -b wt ../gitws/wt)

\$G clone --shared outside-repo gitws/borrowed

g_init gitws/mainwt
printf 'mainwt tracked\n' > gitws/mainwt/tracked.txt
(cd gitws/mainwt && \$G add tracked.txt && \$G commit -q -m "mainwt commit")
printf 'mainwt tracked modified\n' > gitws/mainwt/tracked.txt
printf 'mainwt untracked\n' > gitws/mainwt/untracked.txt
(cd gitws/mainwt && \$G worktree add -b mainwt-branch ../../outside-mainwt-wt)

g_init outer
printf 'outer committed\n' > outer/committed.txt
(cd outer && \$G add committed.txt && \$G commit -q -m "outer commit")
printf 'outer dirty\n' > outer/outer-dirty.txt
printf 'inner file\n' > outer/inner/file.txt
EOF

echo "== connect"
check "the worker answers on stdio" bash -c "'$SNIP' remote ls h $WD | grep -qx edge/"
home=$("$SNIP" remote ls h "~" 2>&1)
check "~ opens the worker's home" test "$?" = 0
refused "a relative workspace" "absolute" ls h relative/dir
refused "a missing workspace" "" ls h $WD/nope
err=$(SNIP_REMOTE_EXEC="$WD/no-such-snip serve --stdio" "$SNIP" remote ls h $WD 2>&1)
check "a worker that cannot start says why ($err)" test -n "$err"
srcname=$(basename "$SRC")

echo "== browsing"
check "folders first" bash -c "'$SNIP' remote ls h $WD/edge | head -1 | grep -q '/\$'"
check "a Chinese name with spaces" bash -c "'$SNIP' remote cat h $WD/edge 'src/deep/中文 有空白.txt' | grep -q 深層"
check "stat of an empty file" bash -c "'$SNIP' remote stat h $WD/edge empty.txt | grep -q '^file	0	'"
check "exactly 1 MiB is served" test "$("$SNIP" remote cat h $WD/edge exact-1MiB.txt | wc -c | tr -d ' ')" = 1048576
check "1200 entries are cut at 1000" test "$("$SNIP" remote ls h $WD/edge manydir 2>/dev/null | wc -l | tr -d ' ')" = 1000
check "a nested repo is a folder" bash -c "'$SNIP' remote stat h $WD/edge nested | grep -q '^directory'"
if w <<<"[ -L '$WD/edge/inner-link' ]"; then
	check "a symlink inside the workspace is followed" bash -c "'$SNIP' remote ls h $WD/edge inner-link | grep -qx deep/"
	check "a folder symlink inside the workspace lists as a folder" bash -c "'$SNIP' remote ls h $WD/edge | grep -qx inner-link/"
else
	skip "a symlink inside the workspace is followed" "no symlinks on the worker"
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
		if [ "$("$SNIP" remote cat h "$ws" "$f" | hash_of)" != "$sum" ]; then
			badfiles=$((badfiles + 1))
			echo "      differs: $f"
		fi
	done <<<"$sums"
	check "$total files in $ws identical to the worker's bytes" test "$badfiles" = 0 -a "$total" -gt 0
}
compare "$WD/text" "$WD/text" .
check "CRLF kept" test "$("$SNIP" remote cat h $WD/edge crlf.txt | hash_of)" = "$(printf 'line1\r\nline2\r\n' | hash_of)"
if [ -n "$SRC" ]; then
	compare "$SRC" "$SRC" "crates docs scripts fixtures -size -1024k \\( -name '*.rs' -o -name '*.md' -o -name '*.toml' -o -name '*.py' -o -name '*.sh' -o -name '*.json' \\)"
fi

echo "== refused"
refused "binary" "binary or not UTF-8" cat h $WD/edge blob.bin
refused "not UTF-8" "binary or not UTF-8" cat h $WD/edge latin.txt
refused "over 1 MiB" "exceeds 1 MiB" cat h $WD/edge over-1MiB.txt
refused "../" "inside the workspace" cat h $WD/edge ../secret.txt
refused "src/../../" "inside the workspace" cat h $WD/edge src/../../secret.txt
refused "an absolute path" "inside the workspace" cat h $WD/edge "$WD/secret.txt"
refused "ls .." "inside the workspace" ls h $WD/edge ..
refused "a folder read as a file" "" cat h $WD/edge src
if w <<<"[ -L '$WD/edge/escape.txt' ]"; then
	refused "a symlink out of the workspace" "leaves the workspace" cat h $WD/edge escape.txt
	check "a folder symlink out of the workspace lists as a plain entry" bash -c "'$SNIP' remote ls h $WD/edge | grep -qx escape-dir"
	refused "a folder symlink out of the workspace" "leaves the workspace" ls h $WD/edge escape-dir
else
	skip "a symlink out of the workspace" "no symlinks on the worker"
fi

echo "== parallel"
want=$("$SNIP" remote cat h $WD/text plain_60.txt | hash_of)
# Each command is a worker process; over ssh each is a login too, and sshd
# drops logins beyond MaxStartups (10 by default).
counts="20 50 100"
[ -n "$HOST" ] && counts="4 8"
for n in $counts; do
	rm -f "$MASTER"/par.*
	for i in $(seq 1 "$n"); do
		("$SNIP" remote cat h $WD/text plain_60.txt 2>/dev/null | hash_of >"$MASTER/par.$i") &
	done
	wait
	good=$(cat "$MASTER"/par.* | grep -c "$want")
	check "$n parallel reads all correct ($good/$n)" test "$good" = "$n"
done

echo "== git views"
w <<<"sleep 1 && touch '$WD/gitws/alpha/b.txt'"
alpha_index_before=$(w <<<"(command -v sha256sum >/dev/null && sha256sum < '$WD/gitws/alpha/.git/index' || shasum -a 256 < '$WD/gitws/alpha/.git/index') | cut -c1-64")

repos_gitws=$("$SNIP" remote repos h $WD/gitws)
check "repos gitws lists alpha and beta" test "$(grep -c '^alpha	' <<<"$repos_gitws")" = 1 -a "$(grep -c '^beta	' <<<"$repos_gitws")" = 1
check "wt, broken, borrowed appear as error rows" test "$(grep -c '^wt	error: ' <<<"$repos_gitws")" = 1 -a "$(grep -c '^broken	error: ' <<<"$repos_gitws")" = 1 -a "$(grep -c '^borrowed	error: ' <<<"$repos_gitws")" = 1
check "no error row contains the outside path" test "$(grep -c "outside-repo" <<<"$repos_gitws")" = 0

changes_mainwt=$("$SNIP" remote changes h $WD/gitws mainwt)
log_mainwt=$("$SNIP" remote log h $WD/gitws mainwt)
all_mainwt="$repos_gitws
$changes_mainwt
$log_mainwt"
check "main repo with a worktree outside the share is served" test "$(grep -c '^mainwt	' <<<"$repos_gitws")" = 1 \
	-a "$(grep '^mainwt	' <<<"$repos_gitws" | grep -c 'error:')" = 0 \
	-a "$(grep -c '	tracked.txt$' <<<"$changes_mainwt")" -ge 1 \
	-a "$(grep -c 'untracked.txt' <<<"$changes_mainwt")" -ge 1 \
	-a "$(grep -c 'mainwt commit' <<<"$log_mainwt")" -ge 1 \
	-a "$(grep -c 'outside-mainwt-wt' <<<"$all_mainwt")" = 0

alpha_line=$(grep '^alpha	' <<<"$repos_gitws")
alpha_counts=$(cut -f3,4,5 <<<"$alpha_line")
oracle_counts=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$WD/gitws/alpha' status --porcelain=v2" | awk '/^1/ || /^2/ { if (substr($2, 1, 1) != ".") staged++; if (substr($2, 2, 1) != ".") unstaged++; } /^\?/ { untracked++ } END { printf "%d\t%d\t%d\n", staged+0, unstaged+0, untracked+0 }')
check "alpha status counts match oracle ($alpha_counts)" test "$alpha_counts" = "$oracle_counts" -a -n "$alpha_counts"

changes_alpha=$("$SNIP" remote changes h $WD/gitws alpha)
changes_paths=$(cut -f3 <<<"$changes_alpha" | LC_ALL=C sort)
oracle_changes_paths=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$WD/gitws/alpha' status --porcelain=v2 --untracked-files=all" | awk '/^1/ || /^2/ { print $9 } /^\?/ { print $2 }' | LC_ALL=C sort)
check "changes alpha path set == oracle path set" test "$changes_paths" = "$oracle_changes_paths" -a -n "$changes_paths"

changes_beta=$("$SNIP" remote changes h $WD/gitws beta 2>&1)
rc=$?
check "changes beta prints nothing and exits 0" test "$rc" = 0 -a -z "$changes_beta"

repos_inner_out=$("$SNIP" remote repos h $WD/outer/inner)
repos_inner_all=$("$SNIP" remote repos h $WD/outer/inner 2>&1)
rc=$?
check "repos inner prints no repo rows and does not contain outer-dirty" test "$rc" = 0 -a -z "$repos_inner_out" -a "$(grep -c outer-dirty <<<"$repos_inner_all")" = 0 -a "$(grep -c 'no Git repository in' <<<"$repos_inner_all")" -ge 1
refused "changes inner exits 1" "" changes h $WD/outer/inner

log_alpha=$("$SNIP" remote log h $WD/gitws alpha -n 50)
log_shas=$(cut -f1 <<<"$log_alpha" | LC_ALL=C sort)
oracle_rev_shas=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$WD/gitws/alpha' rev-list --all" | LC_ALL=C sort)
check "log alpha sha set == git rev-list --all" test "$log_shas" = "$oracle_rev_shas" -a -n "$log_shas"

alpha_head=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$WD/gitws/alpha' rev-parse HEAD")
show_alpha=$("$SNIP" remote show h $WD/gitws alpha "$alpha_head")
show_paths=$(cut -f2 <<<"$show_alpha" | LC_ALL=C sort)
oracle_diff_paths=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$WD/gitws/alpha' diff-tree --no-commit-id --name-only -r HEAD" | LC_ALL=C sort)
check "show alpha HEAD path set == diff-tree path set" test "$show_paths" = "$oracle_diff_paths" -a -n "$show_paths"

diff_a=$("$SNIP" remote diff h $WD/gitws alpha a.txt)
check "diff alpha a.txt contains changed line" grep -q "commit 2 a modified" <<<"$diff_a"
diff_staged=$("$SNIP" remote diff h $WD/gitws alpha staged.txt --staged)
check "diff alpha staged.txt --staged contains staged content" grep -q "staged file content" <<<"$diff_staged"

refused "diff ../../secret.txt" "" diff h $WD/gitws alpha ../../secret.txt
if w <<<"[ -L '$WD/gitws/alpha/link-to-secret' ]"; then
	link_out=$("$SNIP" remote diff h $WD/gitws alpha link-to-secret 2>&1)
	rc=$?
	if [ "$rc" = 1 ] && ! grep -q "TOPSECRET" <<<"$link_out"; then
		ok "diff link-to-secret  ($link_out)"
	else
		bad "diff link-to-secret  rc=$rc out=$link_out"
	fi
else
	skip "diff link-to-secret" "no symlinks on the worker"
fi
show_pwned_out=$("$SNIP" remote show h $WD/gitws alpha -- "--output=$WD/pwned" 2>&1)
rc=$?
if [ "$rc" = 1 ] && w <<<"test ! -e '$WD/pwned'"; then
	ok "show --output refused without creating file  ($show_pwned_out)"
else
	bad "show --output  rc=$rc out=$show_pwned_out"
fi
refused "show invalid revision ':/x'" "" show h $WD/gitws alpha ':/x'

wt_err=$("$SNIP" remote changes h $WD/gitws wt 2>&1 >/dev/null)
rc=$?
if [ "$rc" = 1 ] && grep -iq "outside" <<<"$wt_err"; then
	ok "changes on linked worktree refused  ($wt_err)"
else
	bad "changes on linked worktree  rc=$rc err=$wt_err"
fi

# A diff that covers the stat-dirty b.txt is what makes git refresh an index it may write.
"$SNIP" remote diff h $WD/gitws alpha b.txt >/dev/null 2>&1 || true
alpha_index_after=$(w <<<"(command -v sha256sum >/dev/null && sha256sum < '$WD/gitws/alpha/.git/index' || shasum -a 256 < '$WD/gitws/alpha/.git/index') | cut -c1-64")
check "alpha .git/index sha256 unchanged" test "$alpha_index_after" = "$alpha_index_before" -a -n "$alpha_index_after"
check "alpha .git/index.lock does not exist" w <<<"test ! -e '$WD/gitws/alpha/.git/index.lock'"

if [ -n "$SRC" ]; then
	if w <<<"test -d '$SRC/.git'"; then
		src_log=$("$SNIP" remote log h "$SRC" -n 3)
		src_log_shas=$(cut -f1 <<<"$src_log")
		oracle_revs=$(w <<<"GIT_OPTIONAL_LOCKS=0 git --no-optional-locks -C '$SRC' rev-list --all")
		all_found=true
		while IFS= read -r sha; do
			[ -n "$sha" ] || continue
			if ! grep -q "^$sha" <<<"$oracle_revs"; then
				all_found=false
				break
			fi
		done <<<"$src_log_shas"
		check "log $srcname -n 3 shas contained in rev-list --all" test "$all_found" = true -a -n "$src_log_shas"
	elif w <<<"test -f '$SRC/.git'"; then
		src_repos=$("$SNIP" remote repos h "$SRC" 2>&1)
		check "repos $srcname reports an error row for linked worktree" grep -q "error: " <<<"$src_repos"
	else
		src_repos=$("$SNIP" remote repos h "$SRC" 2>&1)
		check "repos $srcname reports no Git repository for archive" grep -q "no Git repository in" <<<"$src_repos"
	fi
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
