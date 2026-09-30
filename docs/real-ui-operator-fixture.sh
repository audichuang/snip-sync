#!/bin/sh
# Gate B fixtures for docs/real-ui-operator-protocol.md.
# Usage: docs/real-ui-operator-fixture.sh <empty-directory>
set -eu

ROOT=${1:?usage: docs/real-ui-operator-fixture.sh <empty-directory>}
if [ -e "$ROOT" ] && [ -n "$(ls -A "$ROOT" 2>/dev/null || true)" ]; then
	echo "refusing non-empty directory: $ROOT" >&2
	exit 2
fi
mkdir -p "$ROOT"
FIX="$ROOT/fixtures"

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME='QA Operator'
export GIT_AUTHOR_EMAIL='qa@example.com'
export GIT_COMMITTER_NAME='QA Operator'
export GIT_COMMITTER_EMAIL='qa@example.com'

g() {
	dir=$1
	shift
	git -C "$dir" "$@"
}

init_repo() {
	dir=$1
	mkdir -p "$dir"
	git init -b main "$dir" >/dev/null
	g "$dir" config user.name 'QA Operator'
	g "$dir" config user.email 'qa@example.com'
	g "$dir" config commit.gpgsign false
}

at() {
	export GIT_AUTHOR_DATE=$1
	export GIT_COMMITTER_DATE=$2
}

commit() {
	dir=$1
	at "$2" "$3"
	shift 3
	g "$dir" add -A
	g "$dir" commit -m "$*" >/dev/null
}

i=1
while [ "$i" -le 15 ]; do
	n=$(printf 'repo%02d' "$i")
	init_repo "$FIX/ws-src/$n"
	init_repo "$FIX/ws-dst/$n"
	printf 'base %s\n' "$n" > "$FIX/ws-src/$n/README.md"
	printf 'base %s\n' "$n" > "$FIX/ws-dst/$n/README.md"
	commit "$FIX/ws-src/$n" '2026-01-01T00:00:00+08:00' '2026-01-01T00:01:00+08:00' "base $n"
	commit "$FIX/ws-dst/$n" '2026-01-01T00:00:00+08:00' '2026-01-01T00:01:00+08:00' "base $n"
	i=$((i + 1))
done

printf 'from-repo01\n' > "$FIX/ws-src/repo01/unrelated.txt"
g "$FIX/ws-src/repo01" add unrelated.txt
printf 'from-repo03\n' > "$FIX/ws-src/repo03/unrelated.txt"
g "$FIX/ws-src/repo03" add unrelated.txt
printf 'dest-old-01\n' > "$FIX/ws-dst/repo01/unrelated.txt"
commit "$FIX/ws-dst/repo01" '2026-01-02T00:00:00+08:00' '2026-01-02T00:01:00+08:00' 'dest unrelated'
printf 'dest-old-03\n' > "$FIX/ws-dst/repo03/unrelated.txt"
commit "$FIX/ws-dst/repo03" '2026-01-02T00:00:00+08:00' '2026-01-02T00:01:00+08:00' 'dest unrelated'

printf 'not-a-directory\n' > "$FIX/ws-dst/repo04/newdir"
commit "$FIX/ws-dst/repo04" '2026-01-02T00:00:00+08:00' '2026-01-02T00:01:00+08:00' 'newdir is a file'

printf 'create-a\n' > "$FIX/ws-src/repo02/only-src.txt"
g "$FIX/ws-src/repo02" add only-src.txt
printf 'create-b\n' > "$FIX/ws-src/repo05/only-src.txt"
g "$FIX/ws-src/repo05" add only-src.txt
printf 'src-over\n' > "$FIX/ws-src/repo06/shared.txt"
g "$FIX/ws-src/repo06" add shared.txt
printf 'dst-over\n' > "$FIX/ws-dst/repo06/shared.txt"
commit "$FIX/ws-dst/repo06" '2026-01-03T00:00:00+08:00' '2026-01-03T00:01:00+08:00' 'shared exists'
printf 'src-over-2\n' > "$FIX/ws-src/repo07/shared.txt"
g "$FIX/ws-src/repo07" add shared.txt
printf 'dst-over-2\n' > "$FIX/ws-dst/repo07/shared.txt"
commit "$FIX/ws-dst/repo07" '2026-01-03T00:00:00+08:00' '2026-01-03T00:01:00+08:00' 'shared exists'

mkdir -p "$FIX/nongit-dst"

init_repo "$FIX/files-src"
init_repo "$FIX/files-dst"
mkdir -p "$FIX/files-src/folder" "$FIX/files-dst"
printf 'keep\n' > "$FIX/files-src/README.md"
printf 'old\n' > "$FIX/files-src/old-name.txt"
printf 'gone\n' > "$FIX/files-src/gone.txt"
printf 'both-base\n' > "$FIX/files-src/both.txt"
printf 'a\0b\n' > "$FIX/files-src/binary.dat"
commit "$FIX/files-src" '2026-02-01T00:00:00+08:00' '2026-02-01T00:01:00+08:00' 'file base'

printf 'staged-new\n' > "$FIX/files-src/staged-new.txt"
g "$FIX/files-src" add staged-new.txt
printf 'index-body\n' > "$FIX/files-src/both.txt"
g "$FIX/files-src" add both.txt
printf 'worktree-body\n' > "$FIX/files-src/both.txt"
g "$FIX/files-src" rm gone.txt >/dev/null
g "$FIX/files-src" mv old-name.txt new-name.txt

printf 'folder-a\n' > "$FIX/files-src/folder/a.txt"
printf 'folder-b\n' > "$FIX/files-src/folder/b.txt"
printf 'folder-c\n' > "$FIX/files-src/folder/c.txt"
printf 'folder-d\n' > "$FIX/files-src/folder/d.txt"
printf 'folder-e\n' > "$FIX/files-src/folder/e.txt"
printf 'bin\0\n' > "$FIX/files-src/folder/bin.dat"
python3 -c 'open("'"$FIX"'/files-src/folder/utf16.txt","wb").write("文字".encode("utf-16"))'
python3 -c 'open("'"$FIX"'/files-src/large.txt","wb").write(b"x"*1360000)'
printf '' > "$FIX/files-src/empty.txt"
mkdir -p "$FIX/files-src/路徑 有空白"
printf '中文\n' > "$FIX/files-src/路徑 有空白/檔案.txt"
printf 'line\r\n\r\n' > "$FIX/files-src/crlf.txt"

printf 'dest-base\n' > "$FIX/files-dst/README.md"
printf 'do-not-touch\n' > "$FIX/files-dst/overwrite.txt"
printf 'dest-both\n' > "$FIX/files-dst/both.txt"
printf 'gone-dest\n' > "$FIX/files-dst/gone.txt"
printf 'old-dest\n' > "$FIX/files-dst/old-name.txt"
printf 'keep-bin\n' > "$FIX/files-dst/binary.dat"
mkdir -p "$FIX/files-dst/folder"
printf 'keep-utf16\n' > "$FIX/files-dst/folder/utf16.txt"
commit "$FIX/files-dst" '2026-02-01T00:00:00+08:00' '2026-02-01T00:01:00+08:00' 'dest base'
printf 'will-overwrite\n' > "$FIX/files-src/overwrite.txt"

init_repo "$FIX/commits-src"
printf 'base\n' > "$FIX/commits-src/common.txt"
printf 'old\n' > "$FIX/commits-src/old.txt"
printf 'gone\n' > "$FIX/commits-src/gone.txt"
printf 'a\0b\n' > "$FIX/commits-src/binary.dat"
printf 'readme\n' > "$FIX/commits-src/README.md"
commit "$FIX/commits-src" '2026-03-01T09:00:00+08:00' '2026-03-01T09:01:00+08:00' 'commit base'
g "$FIX/commits-src" branch side

printf 'c1\n' > "$FIX/commits-src/common.txt"
at '2026-03-02T09:00:00+08:00' '2026-03-02T09:01:00+08:00'
g "$FIX/commits-src" add common.txt
g "$FIX/commits-src" commit -m "$(printf '多行中文\n\n第二段\n')" >/dev/null

printf 'c2\n' > "$FIX/commits-src/common.txt"
g "$FIX/commits-src" mv old.txt new.txt
g "$FIX/commits-src" rm gone.txt binary.dat >/dev/null
printf '你好 ✨\n' > "$FIX/commits-src/emoji.txt"
at '2026-03-03T09:00:00+08:00' '2026-03-03T09:01:00+08:00'
g "$FIX/commits-src" add -A
g "$FIX/commits-src" commit -m 'C2 rename delete emoji' >/dev/null

g "$FIX/commits-src" checkout side >/dev/null
printf 'from-side\n' > "$FIX/commits-src/side.txt"
commit "$FIX/commits-src" '2026-03-04T09:00:00+08:00' '2026-03-04T09:01:00+08:00' 'SIDE'
g "$FIX/commits-src" checkout main >/dev/null
at '2026-03-05T09:00:00+08:00' '2026-03-05T09:01:00+08:00'
g "$FIX/commits-src" merge --no-ff side -m 'C3 merge' >/dev/null

mkdir -p "$FIX/commits-src/newdir"
printf 'text\n' > "$FIX/commits-src/newdir/content.txt"
printf 'a\0b\n' > "$FIX/commits-src/new-binary.bin"
at '2026-03-06T09:00:00+08:00' '2026-03-06T09:01:00+08:00'
g "$FIX/commits-src" add newdir/content.txt new-binary.bin
g "$FIX/commits-src" commit -m 'C4 text and binary' >/dev/null

at '2026-03-07T09:00:00+08:00' '2026-03-07T09:01:00+08:00'
g "$FIX/commits-src" commit --allow-empty -m 'empty replay' >/dev/null

init_repo "$FIX/commits-dst-overwrite"
printf 'already\n' > "$FIX/commits-dst-overwrite/common.txt"
printf 'readme\n' > "$FIX/commits-dst-overwrite/README.md"
commit "$FIX/commits-dst-overwrite" '2026-03-01T00:00:00+08:00' '2026-03-01T00:01:00+08:00' 'dst base'
g "$FIX/commits-dst-overwrite" checkout -b qa-replay >/dev/null
printf 'staged-keep\n' > "$FIX/commits-dst-overwrite/staged-keep.txt"
g "$FIX/commits-dst-overwrite" add staged-keep.txt
printf 'local-only\n' > "$FIX/commits-dst-overwrite/local-only.txt"

init_repo "$FIX/commits-dst-clean"
printf 'readme\n' > "$FIX/commits-dst-clean/README.md"
commit "$FIX/commits-dst-clean" '2026-03-01T00:00:00+08:00' '2026-03-01T00:01:00+08:00' 'dst clean'
g "$FIX/commits-dst-clean" checkout -b qa-replay >/dev/null

init_repo "$FIX/commits-dst-present"
printf 'readme\n' > "$FIX/commits-dst-present/README.md"
printf 'a\0b\n' > "$FIX/commits-dst-present/binary.dat"
printf 'gone\n' > "$FIX/commits-dst-present/gone.txt"
printf 'old\n' > "$FIX/commits-dst-present/old.txt"
mkdir -p "$FIX/commits-dst-present/dir"
printf 'folder-gone\n' > "$FIX/commits-dst-present/dir/gone.txt"
commit "$FIX/commits-dst-present" '2026-03-01T00:00:00+08:00' '2026-03-01T00:01:00+08:00' 'dst present'
g "$FIX/commits-dst-present" checkout -b qa-replay >/dev/null

init_repo "$FIX/basket-src"
mkdir -p "$FIX/basket-src/dir"
printf 'keep\n' > "$FIX/basket-src/dir/keep.txt"
printf 'folder-gone\n' > "$FIX/basket-src/dir/gone.txt"
commit "$FIX/basket-src" '2026-04-01T09:00:00+08:00' '2026-04-01T09:01:00+08:00' 'basket base'
printf 'keep-2\n' > "$FIX/basket-src/dir/keep.txt"
rm "$FIX/basket-src/dir/gone.txt"
commit "$FIX/basket-src" '2026-04-02T09:00:00+08:00' '2026-04-02T09:01:00+08:00' 'basket folder and delete'

init_repo "$FIX/nonutf8-src"
printf 'readme\n' > "$FIX/nonutf8-src/README.md"
printf 'caf\351\n' > "$FIX/nonutf8-src/latin1.txt"
commit "$FIX/nonutf8-src" '2026-05-01T09:00:00+08:00' '2026-05-01T09:01:00+08:00' 'nonutf8 base'
printf 'caf\351!\n' > "$FIX/nonutf8-src/latin1.txt"
printf 'ok\n' > "$FIX/nonutf8-src/ok.txt"
commit "$FIX/nonutf8-src" '2026-05-02T09:00:00+08:00' '2026-05-02T09:01:00+08:00' 'N1 modify latin1 add ok'
g "$FIX/nonutf8-src" rm latin1.txt >/dev/null
commit "$FIX/nonutf8-src" '2026-05-03T09:00:00+08:00' '2026-05-03T09:01:00+08:00' 'N2 delete latin1'

init_repo "$FIX/nonutf8-dst"
printf 'readme\n' > "$FIX/nonutf8-dst/README.md"
printf 'caf\351\n' > "$FIX/nonutf8-dst/latin1.txt"
printf 'caf\351\n' > "$FIX/nonutf8-dst/common.txt"
commit "$FIX/nonutf8-dst" '2026-05-01T00:00:00+08:00' '2026-05-01T00:01:00+08:00' 'nonutf8 dst'
g "$FIX/nonutf8-dst" checkout -b qa-replay >/dev/null

init_repo "$FIX/blocked-src"
printf 'readme\n' > "$FIX/blocked-src/README.md"
commit "$FIX/blocked-src" '2026-06-01T09:00:00+08:00' '2026-06-01T09:01:00+08:00' 'blocked base'
mkdir -p "$FIX/blocked-src/newdir"
printf 'x\n' > "$FIX/blocked-src/newdir/x.txt"
printf 'fresh\n' > "$FIX/blocked-src/fresh.txt"
commit "$FIX/blocked-src" '2026-06-02T09:00:00+08:00' '2026-06-02T09:01:00+08:00' 'B1 blocked dir and fresh'

# A regular file used as a paste destination (K05-key).
printf 'i am a file\n' > "$FIX/file-dst"

init_repo "$FIX/commits-dst-hooks"
printf 'readme\n' > "$FIX/commits-dst-hooks/README.md"
commit "$FIX/commits-dst-hooks" '2026-03-01T00:00:00+08:00' '2026-03-01T00:01:00+08:00' 'dst hooks'
g "$FIX/commits-dst-hooks" checkout -b qa-replay >/dev/null
marker="$FIX/hook-marker.txt"
for hook in pre-commit prepare-commit-msg commit-msg post-commit; do
	cat > "$FIX/commits-dst-hooks/.git/hooks/$hook" << EOF
#!/bin/sh
echo hooked >> '$marker'
exit 1
EOF
	chmod +x "$FIX/commits-dst-hooks/.git/hooks/$hook"
done

cat > "$ROOT/README.txt" << EOF
Gate B fixtures are under fixtures/.
perf15 is separate:
  python3 scripts/workload_generator.py <this-dir>/perf15 --repos 15 --files 1000 --commits 1000 --refs 30 --quiet
Hook marker path: $marker
EOF

echo "fixture ready: $FIX"
g "$FIX/commits-src" log --oneline --decorate
