#!/usr/bin/env python3
"""Functional two-machine collaboration acceptance fixture.

This is not the standard performance workload and it does not apply snip-sync.
It builds deterministic local Git workspaces plus a truth manifest a future UI
driver can execute. Expected commit trees come from a private Git scratch
replay, not from snip-core.
"""

from __future__ import annotations

import argparse
import base64
import copy
import datetime
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Mapping, Sequence


SCHEMA_VERSION = 1
GENERATOR_REVISION = "2026-09-26.1"
FIXTURE_KIND = "functional-collaboration-acceptance"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
GIT_TIMEOUT_SECONDS = 30
MAX_FILE_BYTES = 1_000_000

COMMITTER_NAME = "Fixture Committer"
COMMITTER_EMAIL = "committer@collab.example"
AUTHORS = (
    ("Ada Lovelace", "ada@collab.example"),
    ("Grace Hopper", "grace@collab.example"),
    ("Ken Thompson", "ken@collab.example"),
    ("Mary Keller", "mary@collab.example"),
)

# pair id, A parent, A basename, B parent, B basename.
# Same basenames live under different parents. Destination basenames differ.
PAIRS: tuple[tuple[str, str, str, str, str], ...] = (
    ("pair-01", "west", "billing", "north", "ledger"),
    ("pair-02", "west", "docs", "north", "handbook"),
    ("pair-03", "west", "api", "north", "service"),
    ("pair-04", "west", "web", "north", "frontend"),
    ("pair-05", "west", "auth", "north", "identity"),
    ("pair-06", "west", "mobile", "north", "ios"),
    ("pair-07", "west", "search", "north", "find"),
    ("pair-08", "west", "gateway", "north", "edge"),
    ("pair-09", "east", "billing", "south", "accounts"),
    ("pair-10", "east", "docs", "south", "manual"),
    ("pair-11", "east", "cli", "south", "tools"),
    ("pair-12", "east", "notify", "south", "alerts"),
    ("pair-13", "east", "shared", "south", "kit"),
    ("pair-14", "east", "admin", "south", "console"),
    ("pair-15", "east", "infra", "south", "edge"),
)

BANNED_GIT = {
    "fetch",
    "pull",
    "push",
    "ls-remote",
    "clone",
    "submodule",
    "request-pull",
    "send-pack",
    "archive",
    "bundle",
}

REPO_ROOT = Path(__file__).resolve().parent.parent


class FixtureSafetyError(ValueError):
    """The output path is not a safe place to create a fixture."""


class GitMissingError(RuntimeError):
    """Git is not available. This is a failure, not a successful skip."""


class GitCommandError(RuntimeError):
    """A bounded local Git command failed."""


class VerificationError(RuntimeError):
    """Live Git or bytes do not match the manifest."""


class CompareError(RuntimeError):
    """A snapshot does not match the planned step outcome."""


def git_available() -> bool:
    try:
        result = subprocess.run(
            ["git", "--version"],
            capture_output=True,
            timeout=GIT_TIMEOUT_SECONDS,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return False
    return result.returncode == 0


def missing_git_disposition(require_all: bool) -> str:
    """Unittest policy. The CLI never turns a missing Git into success."""
    return "fail" if require_all else "skip"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def _iso(unix: int, tz: str) -> str:
    sign = 1 if tz[0] == "+" else -1
    hours = int(tz[1:3])
    minutes = int(tz[3:5])
    offset = datetime.timezone(datetime.timedelta(minutes=sign * (hours * 60 + minutes)))
    return datetime.datetime.fromtimestamp(unix, offset).isoformat()


def file_bytes(pair: str, scope: str, path: str, label: str) -> bytes:
    """UTF-8 payload that survives the file-mode blank-line trim unchanged."""
    raw = f"pair={pair} scope={scope} file={path}\n{label}"
    if "\n\n" in raw or raw.startswith("\n") or raw.endswith("\n"):
        raise RuntimeError(f"file {path} is not file-mode invariant")
    if any(not line.strip() for line in raw.split("\n")):
        raise RuntimeError(f"file {path} has a blank line")
    if raw.lower().lstrip().startswith("file:"):
        raise RuntimeError(f"file {path} looks like a clipboard header")
    return raw.encode("utf-8")


def repo_id(machine: str, parent: str, base: str) -> str:
    prefix = "a" if machine == "A" else "b"
    return f"{prefix}-{parent}-{base}"


def rel_repo(machine: str, parent: str, base: str) -> str:
    root = "machine-a" if machine == "A" else "machine-b"
    return f"{root}/{parent}/{base}"


def origin_rel(pair: str) -> str:
    return f"origins/{pair}.git"


def relative_origin(repo_rel: str, origin: str) -> str:
    return Path(os.path.relpath(origin, repo_rel)).as_posix()


class Completed:
    def __init__(self, code: int, out: bytes, err: bytes) -> None:
        self.code = code
        self.out = out
        self.err = err


class GitSession:
    """Local Git only. Host identity, hooks, and network are not consulted."""

    def __init__(self, log_path: str | None = None) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="snip-collab-git-")
        root = Path(self._tmp.name)
        self.template = root / "template"
        self.template.mkdir()
        self.xdg = root / "xdg"
        self.xdg.mkdir()
        self.empty_config = root / "empty-gitconfig"
        self.empty_config.write_bytes(b"")
        self._log = open(log_path, "w", encoding="utf-8") if log_path else None
        self.commands = 0
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(root / "home"),
            "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "XDG_CONFIG_HOME": str(self.xdg),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(self.empty_config),
            "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_TEMPLATE_DIR": str(self.template),
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_ASKPASS": "true",
            "GCM_INTERACTIVE": "never",
            "GIT_MERGE_AUTOEDIT": "no",
            "GIT_AUTHOR_NAME": COMMITTER_NAME,
            "GIT_AUTHOR_EMAIL": COMMITTER_EMAIL,
            "GIT_AUTHOR_DATE": "1704067200 +0000",
            "GIT_COMMITTER_NAME": COMMITTER_NAME,
            "GIT_COMMITTER_EMAIL": COMMITTER_EMAIL,
            "GIT_COMMITTER_DATE": "1704067260 +0000",
        }
        Path(self.env["HOME"]).mkdir()
        self._dash_c = [
            "-c",
            "safe.directory=*",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.https.allow=never",
            "-c",
            "protocol.http.allow=never",
            "-c",
            "protocol.git.allow=never",
            "-c",
            "protocol.ssh.allow=never",
            "-c",
            "protocol.ext.allow=never",
        ]

    def close(self) -> None:
        if self._log is not None and not self._log.closed:
            self._log.close()
        self._tmp.cleanup()

    def __enter__(self) -> GitSession:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def log(self, text: str) -> None:
        if self._log is not None:
            self._log.write(text.rstrip() + "\n")
            self._log.flush()

    def run(
        self,
        repo: Path | None,
        args: Sequence[str],
        *,
        input_bytes: bytes | None = None,
        check: bool = True,
        timeout: int = GIT_TIMEOUT_SECONDS,
        extra_env: Mapping[str, str] | None = None,
    ) -> Completed:
        if not args:
            raise GitCommandError("empty git argv")
        verb = args[0]
        if verb in BANNED_GIT or verb.startswith("http"):
            raise GitCommandError(f"refusing git command {verb}")
        cmd = ["git", *self._dash_c]
        if repo is not None:
            cmd.extend(["-C", str(repo)])
        cmd.extend(args)
        env = dict(self.env)
        if extra_env:
            env.update(extra_env)
        self.commands += 1
        where = str(repo) if repo is not None else "."
        self.log(f"$ git {' '.join(args)}  # cwd={where}")
        try:
            result = subprocess.run(
                cmd,
                input=input_bytes,
                capture_output=True,
                timeout=timeout,
                check=False,
                env=env,
            )
        except subprocess.TimeoutExpired as exc:
            raise GitCommandError(f"git timed out after {timeout}s: {args[0]}") from exc
        except FileNotFoundError as exc:
            raise GitMissingError("git executable not found") from exc
        completed = Completed(result.returncode, result.stdout, result.stderr)
        if check and completed.code != 0:
            err = completed.err.decode("utf-8", "replace")[-2000:]
            raise GitCommandError(f"git {args[0]} failed ({completed.code}): {err}")
        return completed

    def stdout(self, repo: Path | None, args: Sequence[str], **kwargs: Any) -> str:
        return self.run(repo, args, **kwargs).out.decode("utf-8")

    def blob(self, repo: Path, args: Sequence[str], **kwargs: Any) -> bytes:
        return self.run(repo, args, **kwargs).out

    def rev_parse(self, repo: Path, rev: str) -> str:
        return self.stdout(repo, ["rev-parse", "--verify", rev]).strip()


def _looks_inside_repo(path: Path) -> bool:
    try:
        path.resolve().relative_to(REPO_ROOT)
    except ValueError:
        return False
    return True


def validate_target(target: str) -> tuple[Path, str]:
    """Reject unsafe destinations before creating or deleting anything."""
    if not target or not str(target).strip():
        raise FixtureSafetyError("target directory path is empty")
    raw = Path(target)
    if raw.is_symlink():
        raise FixtureSafetyError(f"target is a symlink: {target}")
    absolute = Path(os.path.abspath(target))
    if _looks_inside_repo(absolute):
        raise FixtureSafetyError("refusing to generate inside the snip-sync worktree")
    probe = absolute
    while True:
        if probe.is_symlink():
            raise FixtureSafetyError(f"symlink ancestor: {probe}")
        if probe.parent == probe:
            break
        probe = probe.parent
    if absolute.exists():
        if absolute.is_symlink():
            raise FixtureSafetyError(f"target is a symlink: {target}")
        if not absolute.is_dir():
            raise FixtureSafetyError(f"target is not a directory: {target}")
        with os.scandir(absolute) as scan:
            if any(True for _ in scan):
                raise FixtureSafetyError(f"target directory is not empty: {target}")
        return absolute, "empty"
    parent = absolute.parent
    if parent.is_symlink() or not parent.is_dir():
        raise FixtureSafetyError(f"parent is missing or is a symlink: {parent}")
    return absolute, "missing"


def create_target(path: Path, status: str) -> bool:
    if status == "missing":
        os.mkdir(path)
        return True
    return False


def discard_created(path: Path) -> None:
    """Remove a directory this process created. Never follow a swapped symlink."""
    if path.is_symlink() or not path.is_dir():
        return
    if os.path.realpath(path) != os.path.abspath(path):
        return
    shutil.rmtree(path)


def _data(payload: bytes) -> bytes:
    return b"data " + str(len(payload)).encode("ascii") + b"\n" + payload


class Stream:
    def __init__(self) -> None:
        self.buf = bytearray()
        self.mark = 0

    def commit(
        self,
        *,
        ref: str,
        pair_index: int,
        n: int,
        message: str,
        parent: int | None = None,
        merges: Sequence[int] = (),
        files: Sequence[tuple[str, bytes]] = (),
        deletes: Sequence[str] = (),
    ) -> int:
        self.mark += 1
        mark = self.mark
        author_name, author_email = AUTHORS[n % len(AUTHORS)]
        author_unix = 1704067200 + pair_index * 86400 + n * 3600
        committer_unix = author_unix + 60
        msg = message if message.endswith("\n") else message + "\n"
        self.buf += f"commit {ref}\nmark :{mark}\n".encode()
        self.buf += (
            f"author {author_name} <{author_email}> {author_unix} +0000\n".encode()
        )
        self.buf += (
            f"committer {COMMITTER_NAME} <{COMMITTER_EMAIL}> {committer_unix} +0000\n".encode()
        )
        self.buf += _data(msg.encode("utf-8"))
        if parent is not None:
            self.buf += f"from :{parent}\n".encode()
        for merge in merges:
            self.buf += f"merge :{merge}\n".encode()
        for path in deletes:
            self.buf += f"D {path}\n".encode()
        for path, content in files:
            self.buf += f"M 644 inline {path}\n".encode()
            self.buf += _data(content)
        return mark

    def reset(self, ref: str, mark: int) -> None:
        self.buf += f"reset {ref}\nfrom :{mark}\n".encode()

    def finish(self) -> bytes:
        self.buf += b"done\n"
        return bytes(self.buf)


def _shared_files(pair: str) -> list[tuple[str, bytes]]:
    paths = [
        "README.txt",
        "docs/read me.txt",
        "src/app.txt",
        "src/keep.txt",
        "src/extra.txt",
        "src/drop_me.txt",
        "src/samepath.txt",
        "src/rename_src.txt",
        "src/conflict.txt",
        "notes/base.txt",
        "transfer/working.txt",
        "transfer/staged.txt",
        "transfer/fixed.txt",
    ]
    return [(path, file_bytes(pair, "shared", path, "v0")) for path in paths]


def build_stream(pair: str, pair_index: int, machine: str | None) -> bytes:
    """machine is None for the bare origin (shared history only)."""
    st = Stream()
    n = 0

    def take() -> int:
        nonlocal n
        current = n
        n += 1
        return current

    files = _shared_files(pair)
    c0 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: root files\n",
        files=files,
    )
    c1 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: update app\n",
        parent=c0,
        files=[("src/app.txt", file_bytes(pair, "shared", "src/app.txt", "v1"))],
    )
    c2 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: update keep\n",
        parent=c1,
        files=[("src/keep.txt", file_bytes(pair, "shared", "src/keep.txt", "v2"))],
    )
    f1 = st.commit(
        ref="refs/heads/feature",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: start feature\n",
        parent=c1,
        files=[("src/feature.txt", file_bytes(pair, "shared", "src/feature.txt", "f1"))],
    )
    f2 = st.commit(
        ref="refs/heads/feature",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: finish feature\n",
        parent=f1,
        files=[("src/feature.txt", file_bytes(pair, "shared", "src/feature.txt", "f2"))],
    )
    m1 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: merge feature into main\n",
        parent=c2,
        merges=(f2,),
        files=[("src/feature.txt", file_bytes(pair, "shared", "src/feature.txt", "f2"))],
    )
    guide = file_bytes(pair, "shared", "notes/base.txt", "v0")
    c3 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: rename notes and delete drop\n",
        parent=m1,
        deletes=("src/drop_me.txt", "notes/base.txt"),
        files=[("notes/guide.txt", guide)],
    )
    c4 = st.commit(
        ref="refs/heads/main",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: edit guide\n",
        parent=c3,
        files=[("notes/guide.txt", file_bytes(pair, "shared", "notes/guide.txt", "v4"))],
    )
    t1 = st.commit(
        ref="refs/heads/topic",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: diverged topic\n",
        parent=c0,
        files=[("src/topic.txt", file_bytes(pair, "shared", "src/topic.txt", "t1"))],
    )
    t2 = st.commit(
        ref="refs/heads/topic",
        pair_index=pair_index,
        n=take(),
        message=f"shared {pair}: topic tip\n",
        parent=t1,
        files=[("src/topic.txt", file_bytes(pair, "shared", "src/topic.txt", "t2"))],
    )
    st.reset("refs/tags/v1", c2)
    st.reset("refs/tags/post-merge", m1)
    st.reset("refs/tags/rename-point", c3)
    st.reset("refs/remotes/origin/main", c4)
    st.reset("refs/remotes/origin/feature", f2)
    st.reset("refs/remotes/origin/topic", t2)
    if machine is None:
        return st.finish()

    scope = machine
    head = "refs/heads/local-a" if machine == "A" else "refs/heads/local-b"
    relay = "relay/from-a.txt" if machine == "A" else "relay/from-b.txt"
    first = st.commit(
        ref=head,
        pair_index=pair_index,
        n=take(),
        message=f"local {machine} {pair}: add relay\n",
        parent=c4,
        files=[(relay, file_bytes(pair, scope, relay, "v1"))],
    )
    second_files = [(relay, file_bytes(pair, scope, relay, "v2"))]
    if not (machine == "A" and pair == "pair-02"):
        second_files.append(
            ("src/keep.txt", file_bytes(pair, scope, "src/keep.txt", "local-v2"))
        )
    else:
        second_files.append(
            ("src/keep.txt", file_bytes(pair, scope, "src/keep.txt", "local-v2"))
        )
        second_files.append(("assets/tiny.bin", b"pair=pair-02 scope=A file=assets/tiny.bin\n\x00"))
    message = (
        f"local {machine} {pair}: second change\n\nKeep the body line.\n"
        if machine == "A"
        else f"local {machine} {pair}: second change\n"
    )
    st.commit(
        ref=head,
        pair_index=pair_index,
        n=take(),
        message=message,
        parent=first,
        files=second_files,
    )
    unmerged_ref = "refs/heads/unmerged-a" if machine == "A" else "refs/heads/unmerged-b"
    unmerged_path = f"relay/{'unmerged-a' if machine == 'A' else 'unmerged-b'}.txt"
    st.commit(
        ref=unmerged_ref,
        pair_index=pair_index,
        n=take(),
        message=f"local {machine} {pair}: unmerged tip\n",
        parent=c2,
        files=[(unmerged_path, file_bytes(pair, scope, unmerged_path, "side"))],
    )
    st.commit(
        ref="refs/heads/topic",
        pair_index=pair_index,
        n=take(),
        message=f"local {machine} {pair}: advance topic past origin\n",
        parent=t2,
        files=[("src/topic.txt", file_bytes(pair, scope, "src/topic.txt", "local-tip"))],
    )
    if machine == "A" and pair == "pair-01":
        o1 = st.commit(
            ref="refs/heads/octo-1",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: octopus side one\n",
            parent=c4,
            files=[("octo/one.txt", file_bytes(pair, scope, "octo/one.txt", "one"))],
        )
        o2 = st.commit(
            ref="refs/heads/octo-2",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: octopus side two\n",
            parent=c4,
            files=[("octo/two.txt", file_bytes(pair, scope, "octo/two.txt", "two"))],
        )
        o3 = st.commit(
            ref="refs/heads/octo-3",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: octopus side three\n",
            parent=c4,
            files=[("octo/three.txt", file_bytes(pair, scope, "octo/three.txt", "three"))],
        )
        st.commit(
            ref="refs/heads/octopus-demo",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: octopus merge\n",
            parent=c4,
            merges=(o1, o2, o3),
            files=[
                ("octo/one.txt", file_bytes(pair, scope, "octo/one.txt", "one")),
                ("octo/two.txt", file_bytes(pair, scope, "octo/two.txt", "two")),
                ("octo/three.txt", file_bytes(pair, scope, "octo/three.txt", "three")),
            ],
        )
    if machine == "A" and pair == "pair-05":
        root = st.commit(
            ref="refs/heads/roots/mini",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: orphan root\n",
            files=[("root-only.txt", file_bytes(pair, scope, "root-only.txt", "root-v1"))],
        )
        st.commit(
            ref="refs/heads/roots/mini",
            pair_index=pair_index,
            n=take(),
            message=f"local A {pair}: child of root\n",
            parent=root,
            files=[
                ("root-only.txt", file_bytes(pair, scope, "root-only.txt", "root-v2")),
                ("root-child.txt", file_bytes(pair, scope, "root-child.txt", "child")),
            ],
        )
    return st.finish()


def write_repo_config(git_dir: Path, *, bare: bool, remote: str | None) -> None:
    hooks = "no-hooks" if bare else ".git/no-hooks"
    lines = [
        "[core]",
        "\trepositoryformatversion = 0",
        "\tfilemode = false",
        f"\tbare = {'true' if bare else 'false'}",
        "\tlogallrefupdates = true",
        "\tautocrlf = false",
        "\tsafecrlf = false",
        "\tignorecase = false",
        "\tprecomposeunicode = false",
        "\tsymlinks = true",
        f"\thooksPath = {hooks}",
        "[commit]",
        "\tgpgsign = false",
        "[tag]",
        "\tgpgsign = false",
        "[gc]",
        "\tauto = 0",
        "[advice]",
        "\tdetachedHead = false",
        "[merge]",
        "\tconflictStyle = merge",
        "[rerere]",
        "\tenabled = false",
        "[user]",
        f"\tname = {COMMITTER_NAME}",
        f"\temail = {COMMITTER_EMAIL}",
    ]
    if remote is not None:
        lines.extend(
            [
                '[remote "origin"]',
                f"\turl = {remote}",
                "\tfetch = +refs/heads/*:refs/remotes/origin/*",
            ]
        )
    (git_dir / "config").write_text("\n".join(lines) + "\n", encoding="utf-8")


def init_repo(git: GitSession, path: Path, *, bare: bool, remote: str | None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    args = ["init", "-q", "--template", str(git.template), "-b", "main"]
    if bare:
        args.append("--bare")
    args.append(str(path))
    git.run(None, args)
    git_dir = path if bare else path / ".git"
    (git_dir / ("no-hooks" if bare else "no-hooks")).mkdir(exist_ok=True)
    if not bare:
        (path / ".git" / "no-hooks").mkdir(exist_ok=True)
    write_repo_config(git_dir, bare=bare, remote=remote)


def import_stream(git: GitSession, repo: Path, stream: bytes) -> None:
    result = git.run(repo, ["fast-import", "--quiet"], input_bytes=stream, check=False)
    if result.code != 0:
        err = result.err.decode("utf-8", "replace")[-2000:]
        raise GitCommandError(f"fast-import failed: {err}")


def checkout_branch(git: GitSession, repo: Path, branch: str) -> None:
    git.run(repo, ["checkout", "-f", "-q", branch])


def hash_object(git: GitSession, repo: Path, data: bytes) -> str:
    return git.stdout(repo, ["hash-object", "-w", "--stdin"], input_bytes=data).strip()


def update_index(git: GitSession, repo: Path, oid: str, path: str, *, add: bool) -> None:
    args = ["update-index"]
    if add:
        args.append("--add")
    args.extend(["--cacheinfo", f"100644,{oid},{path}"])
    git.run(repo, args)


def read_nofollow(path: Path) -> bytes:
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        raise VerificationError(f"refusing to follow symlink {path}")
    if not stat.S_ISREG(info.st_mode):
        raise VerificationError(f"not a regular file: {path}")
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(path, flags)
    chunks: list[bytes] = []
    total = 0
    try:
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_FILE_BYTES:
                raise VerificationError(f"file exceeds {MAX_FILE_BYTES} bytes: {path}")
            chunks.append(chunk)
    finally:
        os.close(fd)
    return b"".join(chunks)


def write_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(path, flags, 0o644)
    try:
        os.write(fd, data)
    finally:
        os.close(fd)


def apply_dirty(git: GitSession, repo: Path, pair: str, machine: str, ident: str, *, rename: bool, delete: bool) -> None:
    scope = machine
    index_same = file_bytes(pair, f"{machine}-index", "src/samepath.txt", "version-A")
    work_same = file_bytes(pair, f"{machine}-worktree", "src/samepath.txt", "version-B")
    update_index(git, repo, hash_object(git, repo, index_same), "src/samepath.txt", add=False)
    write_bytes(repo / "src/samepath.txt", work_same)

    index_staged = file_bytes(pair, f"{machine}-index", "transfer/staged.txt", "index-A")
    work_staged = file_bytes(pair, f"{machine}-worktree", "transfer/staged.txt", "worktree-B")
    update_index(git, repo, hash_object(git, repo, index_staged), "transfer/staged.txt", add=False)
    write_bytes(repo / "transfer/staged.txt", work_staged)

    write_bytes(
        repo / "transfer/working.txt",
        file_bytes(pair, f"{machine}-worktree", "transfer/working.txt", "worktree-only"),
    )
    hold = file_bytes(pair, "index", "local/hold.txt", ident)
    update_index(git, repo, hash_object(git, repo, hold), "local/hold.txt", add=True)
    write_bytes(repo / "local/hold.txt", hold)
    write_bytes(
        repo / "scratch/untracked.txt",
        file_bytes(pair, scope, "scratch/untracked.txt", "untracked"),
    )
    write_bytes(
        repo / "scratch/identity.txt",
        file_bytes(pair, scope, "scratch/identity.txt", ident),
    )
    if rename:
        oid = git.rev_parse(repo, "HEAD:src/rename_src.txt")
        git.run(repo, ["update-index", "--force-remove", "src/rename_src.txt"])
        update_index(git, repo, oid, "src/rename_dst.txt", add=True)
        data = read_nofollow(repo / "src/rename_src.txt")
        os.remove(repo / "src/rename_src.txt")
        write_bytes(repo / "src/rename_dst.txt", data)
    if delete:
        os.remove(repo / "src/extra.txt")
    write_ignored(repo, pair, machine, ident)


def write_ignored(repo: Path, pair: str, machine: str, ident: str) -> None:
    """One deterministic ignored worktree file. The exclude rule lives in .git."""
    exclude = repo / ".git" / "info" / "exclude"
    exclude.parent.mkdir(parents=True, exist_ok=True)
    exclude.write_text("scratch/ignored.txt\n", encoding="utf-8")
    write_bytes(
        repo / "scratch/ignored.txt",
        file_bytes(pair, machine, "scratch/ignored.txt", ident),
    )


def remove_index_path(git: GitSession, repo: Path, path: str) -> None:
    """Staged deletion: drop the index entry and the worktree file."""
    git.run(repo, ["update-index", "--force-remove", "--", path])
    target = repo / path
    if target.exists() or target.is_symlink():
        os.remove(target)


def apply_conflict(git: GitSession, repo: Path, pair: str, pair_index: int) -> None:
    author_unix = 1704067200 + pair_index * 86400 + 40 * 3600

    def commit_file(label: str, message: str, when: int) -> None:
        name, email = AUTHORS[when % len(AUTHORS)]
        git.run(repo, ["--literal-pathspecs", "add", "--", "src/conflict.txt"])
        git.run(
            repo,
            [
                "--literal-pathspecs",
                "commit",
                "--quiet",
                "--no-verify",
                "--cleanup=verbatim",
                "-F",
                "-",
                "--",
                "src/conflict.txt",
            ],
            input_bytes=message.encode("utf-8"),
            extra_env={
                "GIT_AUTHOR_NAME": name,
                "GIT_AUTHOR_EMAIL": email,
                "GIT_AUTHOR_DATE": f"{when} +0000",
                "GIT_COMMITTER_NAME": COMMITTER_NAME,
                "GIT_COMMITTER_EMAIL": COMMITTER_EMAIL,
                "GIT_COMMITTER_DATE": f"{when + 60} +0000",
            },
        )

    git.run(repo, ["checkout", "-q", "-b", "conflict-theirs"])
    write_bytes(repo / "src/conflict.txt", file_bytes(pair, "A", "src/conflict.txt", "THEIRS"))
    commit_file("THEIRS", f"local A {pair}: conflict theirs\n", author_unix)
    git.run(repo, ["checkout", "-q", "local-a"])
    write_bytes(repo / "src/conflict.txt", file_bytes(pair, "A", "src/conflict.txt", "OURS"))
    commit_file("OURS", f"local A {pair}: conflict ours\n", author_unix + 2)
    result = git.run(repo, ["merge", "--no-commit", "--no-ff", "conflict-theirs"], check=False)
    if result.code == 0:
        raise GitCommandError("expected a real merge conflict on pair-15 A")
    if result.code != 1:
        raise GitCommandError(result.err.decode("utf-8", "replace")[-1500:])
    unmerged = git.blob(repo, ["ls-files", "-u", "-z"])
    if b"src/conflict.txt" not in unmerged:
        raise GitCommandError("merge did not leave src/conflict.txt unmerged")


def parse_ident(raw: bytes) -> dict[str, Any]:
    text = raw.decode("utf-8")
    name, rest = text.split(" <", 1)
    email, rest = rest.split("> ", 1)
    unix_s, tz = rest.split(" ", 1)
    unix = int(unix_s)
    return {
        "name": name,
        "email": email,
        "unix": unix,
        "tz": tz,
        "iso": _iso(unix, tz),
    }


def parse_commit_object(body: bytes) -> dict[str, Any]:
    header, message = body.split(b"\n\n", 1)
    tree = ""
    parents: list[str] = []
    author: dict[str, Any] | None = None
    committer: dict[str, Any] | None = None
    for line in header.split(b"\n"):
        if line.startswith(b"tree "):
            tree = line[5:].decode("ascii")
        elif line.startswith(b"parent "):
            parents.append(line[7:].decode("ascii"))
        elif line.startswith(b"author "):
            author = parse_ident(line[7:])
        elif line.startswith(b"committer "):
            committer = parse_ident(line[10:])
    if not tree or author is None or committer is None:
        raise VerificationError("commit object is missing tree or identity")
    return {
        "tree": tree,
        "parents": parents,
        "author": author,
        "committer": committer,
        "message": message.decode("utf-8"),
    }


def parse_cat_batch(raw: bytes) -> list[tuple[str, str, bytes]]:
    items: list[tuple[str, str, bytes]] = []
    index = 0
    while index < len(raw):
        newline = raw.find(b"\n", index)
        if newline < 0:
            break
        header = raw[index:newline].decode("ascii")
        index = newline + 1
        if header.endswith(" missing"):
            raise VerificationError(f"git object missing: {header}")
        oid, kind, size_s = header.split(" ")
        size = int(size_s)
        body = raw[index : index + size]
        index += size
        if raw[index : index + 1] == b"\n":
            index += 1
        items.append((oid, kind, body))
    return items


def parse_ls_files(raw: bytes) -> list[dict[str, Any]]:
    entries: list[dict[str, Any]] = []
    for record in raw.split(b"\0"):
        if not record:
            continue
        meta, path = record.split(b"\t", 1)
        mode, oid, stage = meta.decode("ascii").split(" ")
        entries.append(
            {
                "mode": mode,
                "oid": oid,
                "stage": int(stage),
                "path": path.decode("utf-8"),
            }
        )
    entries.sort(key=lambda item: (item["path"], item["stage"]))
    return entries


def parse_z_paths(raw: bytes) -> list[str]:
    paths: list[str] = []
    for record in raw.split(b"\0"):
        if record:
            paths.append(record.decode("utf-8"))
    return paths


def parse_refs(raw: bytes) -> list[dict[str, Any]]:
    # Git 2.43 for-each-ref has no -z. Ref names in this fixture are ASCII
    # without tabs or newlines; file paths still use NUL-terminated Git output.
    refs: list[dict[str, Any]] = []
    for line in raw.decode("utf-8").splitlines():
        if not line:
            continue
        name, oid, kind, peeled = line.split("\t")
        refs.append({"name": name, "oid": oid, "type": kind, "peeled": peeled})
    refs.sort(key=lambda item: item["name"])
    return refs


def index_canonical(entries: Sequence[Mapping[str, Any]]) -> str:
    lines = [
        f'{item["mode"]} {item["oid"]} {item["stage"]}\t{item["path"]}\n'
        for item in sorted(entries, key=lambda item: (item["path"], item["stage"]))
    ]
    return _sha256("".join(lines).encode("utf-8"))


def file_record(path: str, data: bytes) -> dict[str, Any]:
    return {"path": path, "sha256": _sha256(data), "size": len(data), "base64": _b64(data)}


def load_repo(git: GitSession, repo: Path, meta: Mapping[str, Any]) -> dict[str, Any]:
    head_oid = git.rev_parse(repo, "HEAD")
    branch = git.stdout(repo, ["symbolic-ref", "-q", "HEAD"]).strip()
    shallow = git.stdout(repo, ["rev-parse", "--is-shallow-repository"]).strip() == "true"
    refs = parse_refs(
        git.blob(
            repo,
            [
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(*objectname)",
            ],
        )
    )
    listed = git.stdout(repo, ["rev-list", "--all", "--parents"])
    ordered: list[str] = []
    parents_from_list: dict[str, list[str]] = {}
    for line in listed.splitlines():
        if not line.strip():
            continue
        parts = line.split()
        ordered.append(parts[0])
        parents_from_list[parts[0]] = parts[1:]
    batch = git.blob(
        repo,
        ["cat-file", "--batch"],
        input_bytes="".join(f"{oid}\n" for oid in ordered).encode("ascii"),
    )
    parsed = {oid: parse_commit_object(body) for oid, kind, body in parse_cat_batch(batch) if kind == "commit"}
    commits: list[dict[str, Any]] = []
    for oid in ordered:
        item = parsed[oid]
        if item["parents"] != parents_from_list[oid]:
            raise VerificationError(f"parent list mismatch for {oid}")
        commits.append(
            {
                "oid": oid,
                "tree": item["tree"],
                "parents": item["parents"],
                "authorName": item["author"]["name"],
                "authorEmail": item["author"]["email"],
                "authorTime": item["author"]["iso"],
                "authorUnix": item["author"]["unix"],
                "committerName": item["committer"]["name"],
                "committerEmail": item["committer"]["email"],
                "committerTime": item["committer"]["iso"],
                "committerUnix": item["committer"]["unix"],
                "message": item["message"],
            }
        )
    commits.sort(key=lambda item: (item["authorUnix"], item["oid"]))
    edges = []
    merges: list[str] = []
    octopuses: list[str] = []
    for item in commits:
        for order, parent in enumerate(item["parents"], start=1):
            edges.append({"child": item["oid"], "parent": parent, "parentOrder": order})
        if len(item["parents"]) == 2:
            merges.append(item["oid"])
        elif len(item["parents"]) >= 3:
            octopuses.append(item["oid"])
    edges.sort(key=lambda item: (item["child"], item["parentOrder"], item["parent"]))
    unmerged_tips: list[str] = []
    for ref in refs:
        if not ref["name"].startswith("refs/heads/"):
            continue
        if ref["name"] == branch:
            continue
        probe = git.run(
            repo,
            ["merge-base", "--is-ancestor", ref["name"], "HEAD"],
            check=False,
        )
        if probe.code != 0:
            unmerged_tips.append(ref["name"])
    unmerged_tips.sort()
    index_entries = parse_ls_files(git.blob(repo, ["ls-files", "-s", "-z"]))
    untracked = parse_z_paths(git.blob(repo, ["ls-files", "-o", "-z", "--exclude-standard"]))
    ignored = parse_z_paths(
        git.blob(repo, ["ls-files", "-o", "-z", "--ignored", "--exclude-standard"])
    )
    paths = sorted({entry["path"] for entry in index_entries} | set(untracked) | set(ignored))
    files: list[dict[str, Any]] = []
    absent: list[str] = []
    for path in paths:
        full = repo / path
        if not full.exists() and not full.is_symlink():
            absent.append(path)
            continue
        files.append(file_record(path, read_nofollow(full)))
    merge = git.run(repo, ["rev-parse", "-q", "--verify", "MERGE_HEAD"], check=False)
    merge_head = merge.out.decode("ascii").strip() if merge.code == 0 else ""
    index_unmerged = any(entry["stage"] != 0 for entry in index_entries) or bool(merge_head)
    count = len(commits)
    if not 10 <= count <= 30:
        raise VerificationError(f"{meta['repoId']} has {count} commits, want 10..30")
    return {
        "repoId": meta["repoId"],
        "machine": meta["machine"],
        "pairId": meta["pairId"],
        "parentDir": meta["parentDir"],
        "basename": meta["basename"],
        "relativePath": meta["relativePath"],
        "counterpartRepoId": meta["counterpartRepoId"],
        "originRelativePath": meta["originRelativePath"],
        "remoteUrl": meta["remoteUrl"],
        "head": {"oid": head_oid, "branch": branch, "shallow": shallow},
        "refs": refs,
        "commits": commits,
        "graph": {
            "edges": edges,
            "mergeCommitOids": merges,
            "octopusCommitOids": octopuses,
            "unmergedTips": unmerged_tips,
        },
        "indexEntries": index_entries,
        "indexEntriesSha256": index_canonical(index_entries),
        "files": files,
        "absentWorktreePaths": absent,
        "indexUnmerged": index_unmerged,
        "mergeHead": merge_head,
        "commitCount": count,
    }


def commit_map(repo: Mapping[str, Any]) -> dict[str, dict[str, Any]]:
    return {item["oid"]: item for item in repo["commits"]}


def select_range(git: GitSession, repo: Path, base_rev: str, tip_rev: str) -> dict[str, Any]:
    base = git.rev_parse(repo, base_rev)
    tip = git.rev_parse(repo, tip_rev)
    raw = git.stdout(repo, ["rev-list", "--first-parent", "--parents", f"{base}..{tip}"])
    lines: list[tuple[str, list[str]]] = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        parts = line.split()
        lines.append((parts[0], parts[1:]))
    if not lines:
        return {
            "legal": False,
            "reason": "empty",
            "baseRev": base_rev,
            "tipRev": tip_rev,
            "baseOid": base,
            "tipOid": tip,
            "oids": [],
            "oldest": "",
            "oldestFirstParent": "",
        }
    oldest, parents = lines[-1]
    first = parents[0] if parents else ""
    oids = [oid for oid, _parents in reversed(lines)]
    return {
        "legal": first == base,
        "reason": "contiguous-first-parent" if first == base else "discontinuous",
        "baseRev": base_rev,
        "tipRev": tip_rev,
        "baseOid": base,
        "tipOid": tip,
        "oids": oids,
        "oldest": oldest,
        "oldestFirstParent": first,
        "includesMerge": any(len(commit_parents) >= 2 for _oid, commit_parents in lines),
        "includesRoot": any(len(commit_parents) == 0 for _oid, commit_parents in lines),
    }


def select_last(git: GitSession, repo: Path, tip_rev: str, count: int) -> dict[str, Any]:
    tip = git.rev_parse(repo, tip_rev)
    shallow = git.stdout(repo, ["rev-parse", "--is-shallow-repository"]).strip() == "true"
    raw = git.stdout(
        repo,
        ["rev-list", "--first-parent", "--parents", "-n", str(count), tip],
    )
    lines: list[tuple[str, list[str]]] = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        parts = line.split()
        lines.append((parts[0], parts[1:]))
    oids = [oid for oid, _parents in reversed(lines)]
    oldest_parents = lines[-1][1] if lines else []
    includes_root = bool(lines) and not oldest_parents
    legal = len(lines) == count and not (includes_root and shallow)
    return {
        "legal": legal,
        "reason": "last-from-tip" if legal else "not-enough-or-shallow",
        "kind": "last-from-tip",
        "tipRev": tip_rev,
        "tipOid": tip,
        "count": count,
        "oids": oids,
        "includesRoot": includes_root,
        "includesMerge": any(len(parents) >= 2 for _oid, parents in lines),
        "shallow": shallow,
    }


def parse_diff_tree_z(raw: bytes) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    index = 0
    while index < len(raw):
        if raw[index : index + 1] == b"\0":
            index += 1
            continue
        if raw[index : index + 1] != b":":
            raise GitCommandError("diff-tree record does not start with colon")
        end = raw.index(b"\0", index)
        header = raw[index + 1 : end].decode("ascii")
        old_mode, new_mode, old_sha, new_sha, status = header.split(" ")
        index = end + 1
        end = raw.index(b"\0", index)
        first = raw[index:end].decode("utf-8")
        index = end + 1
        old_path = ""
        path = first
        if status[:1] in {"R", "C"}:
            end = raw.index(b"\0", index)
            old_path = first
            path = raw[index:end].decode("utf-8")
            index = end + 1
        records.append(
            {
                "oldMode": old_mode,
                "newMode": new_mode,
                "oldSha": old_sha,
                "newSha": new_sha,
                "status": status,
                "oldPath": old_path,
                "path": path,
            }
        )
    return records


def _skip_bytes(data: bytes) -> str:
    if b"\0" in data:
        return "binary"
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        return "non-utf8"
    return ""


def _special_mode(mode: str) -> bool:
    return mode in {"120000", "160000"}


def _remove_path(root: Path, rel: str) -> None:
    full = root / rel
    if full.is_symlink() or full.exists():
        if full.is_dir() and not full.is_symlink():
            raise GitCommandError(f"refusing to delete directory {rel}")
        os.remove(full)


def replay_commits(git: GitSession, src: Path, dst: Path, oids: Sequence[str]) -> dict[str, Any]:
    """Apply first-parent diffs with git commit --only in a private copy of dst."""
    scratch = Path(git._tmp.name) / f"scratch-{_sha256(str(dst).encode())[:8]}-{len(oids)}"
    if scratch.exists():
        shutil.rmtree(scratch)
    shutil.copytree(dst, scratch, symlinks=True)
    before_index = parse_ls_files(git.blob(scratch, ["ls-files", "-s", "-z"]))
    before_files = _read_worktree(scratch)
    new_commits: list[dict[str, Any]] = []
    applied_paths: set[str] = set()
    for oid in oids:
        body = git.blob(src, ["cat-file", "commit", oid])
        source = parse_commit_object(body)
        parent = source["parents"][0] if source["parents"] else EMPTY_TREE
        if parent == EMPTY_TREE:
            git.run(src, ["mktree"], input_bytes=b"", check=False)
        diff = git.blob(
            src,
            [
                "diff-tree",
                "-r",
                "-z",
                "-M",
                "--raw",
                "--no-abbrev",
                "--no-commit-id",
                parent,
                oid,
            ],
        )
        records = parse_diff_tree_z(diff)
        paths: list[str] = []
        not_copied: list[dict[str, str]] = []
        for record in records:
            status = record["status"][:1]
            new_mode = record["newMode"]
            old_mode = record["oldMode"]
            if status == "D":
                if _special_mode(old_mode):
                    not_copied.append({"path": record["path"], "reason": "unsupported-type"})
                    continue
                blob = git.blob(src, ["cat-file", "blob", record["oldSha"]])
                reason = _skip_bytes(blob)
                if reason:
                    not_copied.append({"path": record["path"], "reason": reason})
                    continue
                _remove_path(scratch, record["path"])
                paths.append(record["path"])
                continue
            target = record["path"]
            if _special_mode(new_mode) or (status in {"R", "C"} and _special_mode(old_mode)):
                not_copied.append({"path": target, "reason": "unsupported-type"})
                continue
            blob = git.blob(src, ["cat-file", "blob", record["newSha"]])
            reason = _skip_bytes(blob)
            if reason:
                not_copied.append({"path": target, "reason": reason})
                continue
            if status in {"R", "C"}:
                _remove_path(scratch, record["oldPath"])
                write_bytes(scratch / target, blob)
                paths.extend([record["oldPath"], target])
            else:
                write_bytes(scratch / target, blob)
                paths.append(target)
        deduped: list[str] = []
        for path in paths:
            if path not in deduped:
                deduped.append(path)
            applied_paths.add(path)
        if deduped:
            git.run(scratch, ["--literal-pathspecs", "add", "-A", "-f", "--", *deduped])
        author = source["author"]
        committer = source["committer"]
        git.run(
            scratch,
            [
                "--literal-pathspecs",
                "commit",
                "--quiet",
                "--only",
                "--no-verify",
                "--allow-empty",
                "--cleanup=verbatim",
                "-F",
                "-",
                "--",
                *deduped,
            ],
            input_bytes=source["message"].encode("utf-8"),
            extra_env={
                "GIT_AUTHOR_NAME": author["name"],
                "GIT_AUTHOR_EMAIL": author["email"],
                "GIT_AUTHOR_DATE": f"{author['unix']} {author['tz']}",
                "GIT_COMMITTER_NAME": COMMITTER_NAME,
                "GIT_COMMITTER_EMAIL": COMMITTER_EMAIL,
                "GIT_COMMITTER_DATE": f"{committer['unix']} {committer['tz']}",
            },
        )
        tree = git.rev_parse(scratch, "HEAD^{tree}")
        created = parse_commit_object(git.blob(scratch, ["cat-file", "commit", "HEAD"]))
        if created["message"] != source["message"]:
            raise GitCommandError("replay did not preserve the commit message")
        if created["author"]["iso"] != author["iso"] or created["author"]["name"] != author["name"]:
            raise GitCommandError("replay did not preserve author identity")
        if len(created["parents"]) != 1:
            raise GitCommandError("replay commit must have exactly one parent")
        tree_paths = [item["path"] for item in _ls_tree(git, scratch, "HEAD")]
        if "local/hold.txt" in tree_paths:
            raise GitCommandError("replay committed staged local/hold.txt")
        new_commits.append(
            {
                "sourceOid": oid,
                "tree": tree,
                "treePaths": tree_paths,
                "authorName": author["name"],
                "authorEmail": author["email"],
                "authorTime": author["iso"],
                "message": source["message"],
                "notCopied": not_copied,
                "singleParentOnDestination": True,
                "sourceParentCount": len(source["parents"]),
            }
        )
    after_index = parse_ls_files(git.blob(scratch, ["ls-files", "-s", "-z"]))
    after_files = _read_worktree(scratch)
    absent = _absent_paths(scratch, after_index, after_files)
    final_entries = _ls_tree(git, scratch, "HEAD")
    _assert_untouched_preserved(before_files, after_files, before_index, after_index, applied_paths)
    return {
        "commits": new_commits,
        "indexEntries": after_index,
        "files": after_files,
        "absentWorktreePaths": absent,
        "finalTreeEntries": final_entries,
        "appliedPaths": sorted(applied_paths),
    }


def _read_worktree(repo: Path) -> list[dict[str, Any]]:
    """Inventory regular files that are not inside .git. Uses lstat, never follows links."""
    found: list[dict[str, Any]] = []
    for dirpath, dirnames, filenames in os.walk(repo, followlinks=False):
        current = Path(dirpath)
        dirnames[:] = sorted(
            name for name in dirnames if name != ".git" and not (current / name).is_symlink()
        )
        if ".git" in Path(dirpath).parts:
            continue
        for name in sorted(filenames):
            full = current / name
            if full.is_symlink():
                raise VerificationError(f"unexpected symlink in fixture: {full}")
            rel = full.relative_to(repo).as_posix()
            found.append(file_record(rel, read_nofollow(full)))
    found.sort(key=lambda item: item["path"])
    return found


def _absent_paths(repo: Path, index_entries: Sequence[Mapping[str, Any]], files: Sequence[Mapping[str, Any]]) -> list[str]:
    present = {item["path"] for item in files}
    absent = sorted({entry["path"] for entry in index_entries if entry["stage"] == 0} - present)
    return absent


def _ls_tree(git: GitSession, repo: Path, rev: str) -> list[dict[str, str]]:
    raw = git.blob(repo, ["ls-tree", "-r", "-z", rev])
    entries: list[dict[str, str]] = []
    for record in raw.split(b"\0"):
        if not record:
            continue
        meta, path = record.split(b"\t", 1)
        mode, kind, oid = meta.decode("ascii").split(" ")
        entries.append(
            {"mode": mode, "type": kind, "oid": oid, "path": path.decode("utf-8")}
        )
    entries.sort(key=lambda item: item["path"])
    return entries


def _assert_untouched_preserved(
    before_files: Sequence[Mapping[str, Any]],
    after_files: Sequence[Mapping[str, Any]],
    before_index: Sequence[Mapping[str, Any]],
    after_index: Sequence[Mapping[str, Any]],
    applied: set[str],
) -> None:
    before_map = {item["path"]: item["sha256"] for item in before_files}
    after_map = {item["path"]: item["sha256"] for item in after_files}
    for path, digest in before_map.items():
        if path in applied:
            continue
        if after_map.get(path) != digest:
            raise GitCommandError(f"replay changed an unselected worktree path: {path}")
    for path, digest in after_map.items():
        if path in applied:
            continue
        if before_map.get(path) != digest:
            raise GitCommandError(f"replay created an unselected worktree path: {path}")

    def keyed(entries: Sequence[Mapping[str, Any]]) -> dict[tuple[str, int], tuple[str, str]]:
        return {
            (str(item["path"]), int(item["stage"])): (str(item["mode"]), str(item["oid"]))
            for item in entries
            if item["path"] not in applied
        }

    if keyed(before_index) != keyed(after_index):
        raise GitCommandError("replay changed index entries outside the selected paths")


def _by_id(repos: Sequence[Mapping[str, Any]]) -> dict[str, dict[str, Any]]:
    return {str(item["repoId"]): dict(item) for item in repos}


def _find_repo(repos: Mapping[str, Mapping[str, Any]], repo_id_value: str) -> dict[str, Any]:
    return dict(repos[repo_id_value])


def _blob_at(git: GitSession, repo: Path, spec: str) -> tuple[str, bytes]:
    oid = git.rev_parse(repo, spec)
    return oid, git.blob(repo, ["cat-file", "blob", oid])


def live_deletion(git: GitSession, repo: Path, path: str, kind: str) -> dict[str, Any]:
    """Read a selectable Git deletion. kind is working (index vs worktree) or index (HEAD vs index)."""
    if kind == "working":
        diff_args = ["diff", "--name-status", "-z", "--", path]
        diff_against = "index"
        oid = _rev_or_none(git, repo, f":{path}")
        head_oid = _rev_or_none(git, repo, f"HEAD:{path}")
        in_index = oid is not None
    elif kind == "index":
        diff_args = ["diff", "--cached", "--name-status", "-z", "--", path]
        diff_against = "HEAD"
        head_oid = _rev_or_none(git, repo, f"HEAD:{path}")
        oid = head_oid
        in_index = _rev_or_none(git, repo, f":{path}") is not None
    else:
        raise VerificationError(f"unknown deletion kind {kind}")
    raw = git.blob(repo, diff_args)
    if raw != f"D\0{path}\0".encode() or not oid:
        raise VerificationError(f"{path} is not an exportable Git D row ({kind})")
    if (repo / path).exists() or (repo / path).is_symlink():
        raise VerificationError(f"{path} is still in the worktree")
    if kind == "working" and not in_index:
        raise VerificationError(f"{path} is missing from the index")
    if kind == "index" and in_index:
        raise VerificationError(f"{path} is still staged")
    return {
        "kind": kind,
        "path": path,
        "status": "D",
        "diffAgainst": diff_against,
        "mode": "100644",
        "oid": oid,
        "headOid": head_oid or "",
        "inIndex": in_index,
        "inWorktree": False,
    }


def _rev_or_none(git: GitSession, repo: Path, spec: str) -> str | None:
    result = git.run(repo, ["rev-parse", "-q", "--verify", spec], check=False)
    if result.code != 0:
        return None
    return result.out.decode("ascii").strip()


def _file_step(
    git: GitSession,
    repos: Mapping[str, Mapping[str, Any]],
    root: Path,
    *,
    step_id: str,
    source_id: str,
    dest_id: str,
    note: str,
    operations: list[dict[str, Any]],
) -> dict[str, Any]:
    source = repos[source_id]
    dest = repos[dest_id]
    src_path = root / str(source["relativePath"])
    materialized: list[dict[str, Any]] = []
    for op in operations:
        kind = op["kind"]
        path = op["sourcePath"]
        if path != op.get("destPath", path):
            raise VerificationError(f"{step_id} renames {path}; file steps keep the repo-relative path")
        if kind == "delete":
            provenance = live_deletion(git, src_path, path, op["sourceKind"])
            if not any(item["path"] == path for item in dest["files"]):
                raise VerificationError(f"{step_id} dest already lacks {path}")
            materialized.append(
                {
                    "op": "delete",
                    "source": {"repoId": source_id, **provenance},
                    "dest": {"repoId": dest_id, "path": path},
                    "authorized": True,
                    "expected": "absent",
                }
            )
            continue
        if kind == "working":
            data = read_nofollow(src_path / path)
            oid = ""
        elif kind == "index":
            oid, data = _blob_at(git, src_path, f":{path}")
            work = read_nofollow(src_path / path)
            if work == data:
                raise VerificationError(f"{step_id} index source matches the worktree")
        elif kind == "fixed-oid":
            oid, data = _blob_at(git, src_path, f"{op['rev']}:{path}")
        else:
            raise RuntimeError(kind)
        dest_row = next((item for item in dest["files"] if item["path"] == path), None)
        if dest_row is None:
            raise VerificationError(f"{step_id} dest has no {path} to overwrite")
        digest = _sha256(data)
        if dest_row["sha256"] == digest:
            raise VerificationError(f"{step_id} would not change dest {path}")
        materialized.append(
            {
                "op": "write",
                "source": {
                    "kind": kind,
                    "repoId": source_id,
                    "path": path,
                    "rev": op.get("rev", ""),
                    "oid": oid,
                },
                "dest": {"repoId": dest_id, "path": path},
                "authorized": True,
                "overwrite": True,
                "destBaselineSha256": dest_row["sha256"],
                "expectedSha256": digest,
                "expectedBase64": _b64(data),
                "expectedSize": len(data),
            }
        )
    return {
        "id": step_id,
        "kind": "positive-file",
        "direction": note,
        "writes": True,
        "independentFromOtherSteps": True,
        "sourceRepoId": source_id,
        "destRepoId": dest_id,
        "mapping": {
            "explicitRepoIds": True,
            "sourceRepoId": source_id,
            "destRepoId": dest_id,
            "sourceBasename": source["basename"],
            "destBasename": dest["basename"],
            "sourceParent": source["parentDir"],
            "destParent": dest["parentDir"],
        },
        "operations": materialized,
        "preserved": {
            "head": True,
            "index": True,
            "otherRepos": "unchanged",
            "unselectedFiles": "unchanged",
        },
        "byteContract": (
            "File mode writes the working tree only and keeps each source "
            "repo-relative path. Repo name differences are repo-id mappings, not "
            "per-file renames. Overwrites are authorized only where dest bytes "
            "actually change. Deletions are real Git D rows on the source. "
            "Selected bytes have no leading or trailing blank line and no trailing "
            "newline. HEAD and the index stay as they were."
        ),
    }


def _negative(
    step_id: str,
    reason: str,
    detail: str,
    **extra: Any,
) -> dict[str, Any]:
    step: dict[str, Any] = {
        "id": step_id,
        "kind": "negative",
        "reason": reason,
        "detail": detail,
        "writes": False,
        "expected": "full-baseline-snapshot",
        "independentFromOtherSteps": True,
    }
    step.update(extra)
    return step


def _hold_oid(index_entries: Sequence[Mapping[str, Any]]) -> str:
    matches = [item for item in index_entries if item["path"] == "local/hold.txt" and item["stage"] == 0]
    if len(matches) != 1:
        raise VerificationError("destination is missing staged local/hold.txt")
    return str(matches[0]["oid"])


def build_steps(
    git: GitSession,
    repos: Mapping[str, Mapping[str, Any]],
    root: Path,
) -> list[dict[str, Any]]:
    steps: list[dict[str, Any]] = []
    steps.append(
        _file_step(
            git,
            repos,
            root,
            step_id="file-a-to-b-pair01",
            source_id="a-west-billing",
            dest_id="b-north-ledger",
            note="A to B explicit pair mapping; basenames billing and ledger differ",
            operations=[
                {"kind": "working", "sourcePath": "transfer/working.txt"},
                {"kind": "index", "sourcePath": "transfer/staged.txt"},
                {"kind": "fixed-oid", "sourcePath": "src/keep.txt", "rev": "refs/tags/v1"},
                {"kind": "delete", "sourceKind": "working", "sourcePath": "src/extra.txt"},
            ],
        )
    )
    steps.append(
        _file_step(
            git,
            repos,
            root,
            step_id="file-b-to-a-pair09",
            source_id="b-south-accounts",
            dest_id="a-east-billing",
            note="B to A explicit ids. Basename billing is ambiguous and is not used to choose the dest.",
            operations=[
                {"kind": "index", "sourcePath": "transfer/staged.txt"},
                {"kind": "working", "sourcePath": "transfer/working.txt"},
                {"kind": "delete", "sourceKind": "index", "sourcePath": "notes/guide.txt"},
            ],
        )
    )
    steps.append(
        _file_step(
            git,
            repos,
            root,
            step_id="file-explicit-valid-root-pair01-to-pair15b",
            source_id="a-west-billing",
            dest_id="b-south-edge",
            note="Explicit user mapping onto a valid root that is not the pair counterpart. This is allowed.",
            operations=[
                {"kind": "working", "sourcePath": "transfer/working.txt"},
            ],
        )
    )

    def add_commit(
        step_id: str,
        source_id: str,
        dest_id: str,
        selection: dict[str, Any],
        note: str,
    ) -> None:
        if not selection["legal"] or not selection["oids"]:
            raise VerificationError(f"{step_id} selection is not a legal replay range")
        source = repos[source_id]
        dest = repos[dest_id]
        if dest["indexUnmerged"]:
            raise VerificationError(f"{step_id} positive dest is unmerged")
        result = replay_commits(
            git,
            root / str(source["relativePath"]),
            root / str(dest["relativePath"]),
            selection["oids"],
        )
        hold_before = _hold_oid(dest["indexEntries"])
        hold_after = _hold_oid(result["indexEntries"])
        if hold_before != hold_after:
            raise VerificationError(f"{step_id} did not preserve staged local/hold.txt")
        for commit in result["commits"]:
            if "local/hold.txt" in commit["treePaths"]:
                raise VerificationError(f"{step_id} committed staged local/hold.txt")
        steps.append(
            {
                "id": step_id,
                "kind": "positive-commit",
                "direction": note,
                "writes": True,
                "independentFromOtherSteps": True,
                "sourceRepoId": source_id,
                "destRepoId": dest_id,
                "mapping": {
                    "explicitRepoIds": True,
                    "sourceRepoId": source_id,
                    "destRepoId": dest_id,
                },
                "selection": selection,
                "expectedCommits": result["commits"],
                "expectedIndexEntries": result["indexEntries"],
                "expectedFiles": result["files"],
                "expectedAbsentWorktreePaths": result["absentWorktreePaths"],
                "expectedFinalTreeEntries": result["finalTreeEntries"],
                "appliedPaths": result["appliedPaths"],
                "stagedPreservation": {
                    "path": "local/hold.txt",
                    "indexOid": hold_after,
                    "remainsStaged": True,
                    "absentFromNewCommitTrees": True,
                    "assumption": (
                        "Replay commits only the paths in each first-parent diff "
                        "via git add of those paths and git commit --only. Other "
                        "staged entries, including the new file local/hold.txt, "
                        "stay in the index and out of the new commit trees. "
                        "Unrelated worktree dirt such as src/samepath.txt stays. "
                        "Author name, email, author time, and the full message "
                        "are preserved. The new commit OID, the source parent "
                        "list, and the committer identity/time are not."
                    ),
                },
                "preserved": {
                    "headBranch": dest["head"]["branch"],
                    "otherRefs": "unchanged",
                    "otherRepos": "unchanged",
                    "baselineHeadOid": dest["head"]["oid"],
                },
            }
        )

    add_commit(
        "commit-a-to-b-pair02",
        "a-west-docs",
        "b-north-handbook",
        select_range(git, root / "machine-a/west/docs", "refs/remotes/origin/main", "refs/heads/local-a"),
        "A local-a onto B HEAD. Dest index is dirty. Binary add is not copied.",
    )
    add_commit(
        "commit-b-to-a-unmerged-pair03",
        "b-north-service",
        "a-west-api",
        select_range(git, root / "machine-b/north/service", "refs/tags/v1", "refs/heads/unmerged-b"),
        "B unmerged tip, not HEAD, onto A. Dest has staged and samepath dirt.",
    )
    add_commit(
        "commit-merge-first-parent-pair04",
        "a-west-web",
        "b-north-frontend",
        select_range(git, root / "machine-a/west/web", "refs/tags/v1", "refs/tags/post-merge"),
        "One merge commit. Only the first-parent diff is replayed.",
    )
    add_commit(
        "commit-root-pair05",
        "a-west-auth",
        "b-north-identity",
        select_last(git, root / "machine-a/west/auth", "refs/heads/roots/mini", 2),
        "Root plus child on an unmerged orphan branch, replayed onto B HEAD.",
    )
    add_commit(
        "commit-octopus-first-parent-pair01",
        "a-west-billing",
        "b-north-ledger",
        select_range(
            git,
            root / "machine-a/west/billing",
            "refs/heads/main",
            "refs/heads/octopus-demo",
        ),
        "Octopus tip. The replay range is the first-parent commit only.",
    )
    add_commit(
        "commit-rename-delete-pair08",
        "a-west-gateway",
        "b-north-edge",
        select_range(
            git,
            root / "machine-a/west/gateway",
            "refs/tags/post-merge",
            "refs/tags/rename-point",
        ),
        "Rename plus delete from history, onto B's unmerged-b HEAD which does not contain that commit's tree.",
    )

    a_billing = root / "machine-a/west/billing"
    discontinuous = select_range(git, a_billing, "refs/heads/topic", "refs/heads/local-a")
    if discontinuous["legal"]:
        raise VerificationError("topic..local-a was unexpectedly contiguous")
    steps.append(
        _negative(
            "neg-cross-repo-commits",
            "cross-repository",
            "Commit selection mixes two repositories. Replay stays inside one repo.",
            repos=["a-west-billing", "a-west-docs"],
            tips=[
                {"repoId": "a-west-billing", "rev": "refs/heads/local-a"},
                {"repoId": "a-west-docs", "rev": "refs/heads/local-a"},
            ],
        )
    )
    steps.append(
        _negative(
            "neg-noncontiguous-tips",
            "discontinuous",
            "topic and local-a are two tips in one repo and are not one first-parent range.",
            selection=discontinuous,
        )
    )
    steps.append(
        _negative(
            "neg-mapping-collision",
            "target-collision",
            "Two sources keep the same repo-relative path and collide on one dest file.",
            maps=[
                {
                    "sourceRepoId": "a-west-billing",
                    "sourcePath": "src/app.txt",
                    "destRepoId": "b-north-ledger",
                    "destPath": "src/app.txt",
                },
                {
                    "sourceRepoId": "a-west-docs",
                    "sourcePath": "src/app.txt",
                    "destRepoId": "b-north-ledger",
                    "destPath": "src/app.txt",
                },
            ],
        )
    )
    steps.append(
        _negative(
            "neg-mapping-ambiguous-basename",
            "ambiguous-basename",
            "Basename billing exists under west and east. A plan that supplies only the basename has no dest.",
            basename="billing",
            candidateRepoIds=["a-west-billing", "a-east-billing"],
        )
    )
    steps.append(
        _negative(
            "neg-mapping-missing-destination",
            "missing-destination",
            "The mapped destination repo id is not in either workspace.",
            sourceRepoId="a-west-billing",
            sourcePath="src/app.txt",
            destRepoId="b-does-not-exist",
        )
    )
    source_repo = repos["a-west-billing"]
    working = next(item for item in source_repo["files"] if item["path"] == "transfer/working.txt")
    steps.append(
        _negative(
            "neg-stale-source",
            "stale-source",
            "Source-side preview/selection, then an external source change, then Copy/export freshness. Destination Apply does not recheck the other computer.",
            phase="source-export",
            freshness="source-export",
            expected="pre-action-snapshot-after-setup-diff",
            noWriteSnapshot="immediately-before-action-after-setup-diff",
            baselineRole="generation-truth-only",
            sequence=[
                "capture source preview and selection",
                "deliberate external change on the source",
                "snapshot all repos immediately before Copy/export",
                "Copy/export checks that preview against the existing export contract",
            ],
            contract=(
                "A stale export snapshot is rejected or must be reselected. "
                "Pinned commit content keeps the pinned bytes. "
                "This fixture does not claim every working-tree edit is rejected before Copy; "
                "that follows the existing export contract and needs driver evidence."
            ),
            preview={
                "repoId": "a-west-billing",
                "headOid": source_repo["head"]["oid"],
                "headBranch": source_repo["head"]["branch"],
                "indexEntriesSha256": source_repo["indexEntriesSha256"],
                "path": "transfer/working.txt",
                "contentSha256": working["sha256"],
            },
        )
    )
    target = repos["b-north-ledger"]
    target_file = next(item for item in target["files"] if item["path"] == "src/app.txt")
    steps.append(
        _negative(
            "neg-stale-target",
            "stale-target",
            "Destination paste preview, then a deliberate target change, then Apply refuses. The source machine is not rechecked.",
            phase="destination-apply",
            expected="pre-action-snapshot-after-setup-diff",
            noWriteSnapshot="immediately-before-action-after-setup-diff",
            baselineRole="generation-truth-only",
            sequence=[
                "capture destination paste preview",
                "deliberate change of destination HEAD, index, or selected content",
                "snapshot all repos immediately before Apply",
                "Apply refuses and writes nothing",
            ],
            preview={
                "repoId": "b-north-ledger",
                "headOid": target["head"]["oid"],
                "headBranch": target["head"]["branch"],
                "indexEntriesSha256": target["indexEntriesSha256"],
                "path": "src/app.txt",
                "contentSha256": target_file["sha256"],
            },
        )
    )
    steps.append(
        _negative(
            "neg-overwrite-unauthorized",
            "overwrite-unauthorized",
            "src/app.txt already exists and overwrite is not authorized, so the whole step writes nothing.",
            operation={
                "op": "write",
                "sourceRepoId": "a-west-billing",
                "sourcePath": "src/app.txt",
                "sourceKind": "working",
                "destRepoId": "b-north-ledger",
                "destPath": "src/app.txt",
                "authorized": False,
                "destExistsInBaseline": True,
            },
        )
    )
    steps.append(
        _negative(
            "neg-cancel",
            "cancel",
            "Cancel or Escape after preview writes nothing and leaves every repo at the baseline snapshot.",
        )
    )
    return steps


def canonical_dataset_hash(manifest: Mapping[str, Any]) -> str:
    cloned = copy.deepcopy(dict(manifest))
    cloned["datasetHash"] = ""
    payload = json.dumps(cloned, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return _sha256(payload.encode("utf-8"))


def _assert_portable(value: Any, absolute: str) -> None:
    if isinstance(value, str):
        if absolute and absolute in value:
            raise VerificationError("manifest contains the absolute fixture path")
        if value.startswith(("/home/", "/tmp/", "/Users/")):
            raise VerificationError(f"manifest contains an absolute path: {value}")
        if "://" in value:
            raise VerificationError(f"manifest contains a URL: {value}")
    elif isinstance(value, dict):
        for item in value.values():
            _assert_portable(item, absolute)
    elif isinstance(value, list):
        for item in value:
            _assert_portable(item, absolute)


def build_manifest(git: GitSession, root: Path, *, with_origins: bool) -> dict[str, Any]:
    repos: list[dict[str, Any]] = []
    origins: list[dict[str, Any]] = []
    for index, (pair, a_parent, a_base, b_parent, b_base) in enumerate(PAIRS):
        a_rel = rel_repo("A", a_parent, a_base)
        b_rel = rel_repo("B", b_parent, b_base)
        o_rel = origin_rel(pair) if with_origins else ""
        remote_a = relative_origin(a_rel, o_rel) if with_origins else None
        remote_b = relative_origin(b_rel, o_rel) if with_origins else None
        if with_origins:
            origin_path = root / o_rel
            init_repo(git, origin_path, bare=True, remote=None)
            import_stream(git, origin_path, build_stream(pair, index, None))
            origins.append(
                {
                    "pairId": pair,
                    "relativePath": o_rel,
                    "bare": True,
                    "headOid": git.rev_parse(origin_path, "refs/heads/main"),
                }
            )
        for machine, rel, parent, base, remote, counterpart_parent, counterpart_base in (
            ("A", a_rel, a_parent, a_base, remote_a, b_parent, b_base),
            ("B", b_rel, b_parent, b_base, remote_b, a_parent, a_base),
        ):
            path = root / rel
            init_repo(git, path, bare=False, remote=remote)
            import_stream(git, path, build_stream(pair, index, machine))
            head = "local-a" if machine == "A" else "local-b"
            if machine == "B" and pair == "pair-08":
                head = "unmerged-b"
            checkout_branch(git, path, head)
            c3 = git.rev_parse(path, "refs/tags/rename-point")
            author_unix = 1704067200 + index * 86400 + 30 * 3600
            git.run(
                path,
                ["tag", "-a", "v-annotated", c3, "-m", f"annotated {pair}"],
                extra_env={
                    "GIT_COMMITTER_NAME": COMMITTER_NAME,
                    "GIT_COMMITTER_EMAIL": COMMITTER_EMAIL,
                    "GIT_COMMITTER_DATE": f"{author_unix} +0000",
                },
            )
            ident = repo_id(machine, parent, base)
            if not (machine == "A" and pair == "pair-15"):
                apply_dirty(
                    git,
                    path,
                    pair,
                    machine,
                    ident,
                    rename=machine == "A" and pair == "pair-06",
                    delete=machine == "A" and pair == "pair-07",
                )
                if machine == "A" and pair == "pair-01":
                    os.remove(path / "src/extra.txt")
                if machine == "B" and pair == "pair-09":
                    remove_index_path(git, path, "notes/guide.txt")
            else:
                apply_conflict(git, path, pair, index)
                write_bytes(
                    path / "scratch/identity.txt",
                    file_bytes(pair, machine, "scratch/identity.txt", ident),
                )
                write_ignored(path, pair, machine, ident)
        git.log(f"built {pair}")
    for index, (pair, a_parent, a_base, b_parent, b_base) in enumerate(PAIRS):
        for machine, parent, base, other_parent, other_base in (
            ("A", a_parent, a_base, b_parent, b_base),
            ("B", b_parent, b_base, a_parent, a_base),
        ):
            rel = rel_repo(machine, parent, base)
            meta = {
                "repoId": repo_id(machine, parent, base),
                "machine": machine,
                "pairId": pair,
                "parentDir": parent,
                "basename": base,
                "relativePath": rel,
                "counterpartRepoId": repo_id("B" if machine == "A" else "A", other_parent, other_base),
                "originRelativePath": origin_rel(pair) if with_origins else "",
                "remoteUrl": relative_origin(rel, origin_rel(pair)) if with_origins else "",
            }
            repos.append(load_repo(git, root / rel, meta))
    repos.sort(key=lambda item: item["repoId"])
    by_repo = _by_id(repos)
    steps = build_steps(git, by_repo, root)
    pairs = []
    for pair, a_parent, a_base, b_parent, b_base in PAIRS:
        pairs.append(
            {
                "pairId": pair,
                "aRepoId": repo_id("A", a_parent, a_base),
                "bRepoId": repo_id("B", b_parent, b_base),
                "aRelativePath": rel_repo("A", a_parent, a_base),
                "bRelativePath": rel_repo("B", b_parent, b_base),
                "aParent": a_parent,
                "bParent": b_parent,
                "aBasename": a_base,
                "bBasename": b_base,
                "destinationBasenameDiffers": a_base != b_base,
            }
        )
    collisions: dict[tuple[str, str], list[str]] = {}
    for repo in repos:
        key = (str(repo["machine"]), str(repo["basename"]))
        collisions.setdefault(key, []).append(str(repo["repoId"]))
    basename_collisions = [
        {"machine": machine, "basename": base, "repoIds": sorted(ids)}
        for (machine, base), ids in sorted(collisions.items())
        if len(ids) > 1
    ]
    counts = [int(repo["commitCount"]) for repo in repos]
    manifest: dict[str, Any] = {
        "schemaVersion": SCHEMA_VERSION,
        "generatorRevision": GENERATOR_REVISION,
        "kind": FIXTURE_KIND,
        "standardBenchmark": False,
        "datasetHash": "",
        "assumptions": {
            "label": "Functional collaboration acceptance fixture. Not a standard performance benchmark.",
            "fileMode": (
                "Paste writes working-tree bytes only. HEAD, refs, and the index stay "
                "unchanged. Selected bytes in this fixture are invariant under the "
                "file-mode leading/trailing blank-line trim."
            ),
            "commitMode": (
                "Each new commit preserves author name, email, author time, the full "
                "message, and the tree from applying that commit's first-parent diff. "
                "Commit OID, source parents, committer name, and committer time are "
                "not preserved. git commit --only keeps unrelated staged entries."
            ),
            "stepsAreIndependent": True,
            "explicitMappingToAnotherValidRootIsAllowed": True,
            "wrongMappingMeans": ["target-collision", "ambiguous-basename", "missing-destination"],
        },
        "authors": [{"name": name, "email": email} for name, email in AUTHORS],
        "committer": {"name": COMMITTER_NAME, "email": COMMITTER_EMAIL, "preservedOnReplay": False},
        "clock": "fixed author and committer timestamps; not the wall clock",
        "pathStyle": "posix-relative-to-fixture-root",
        "summary": {
            "repoCount": len(repos),
            "worktreeCount": len(repos),
            "reposPerMachine": 15,
            "originCount": len(origins),
            "commitCount": sum(counts),
            "refCount": sum(len(repo["refs"]) for repo in repos),
            "positiveStepCount": sum(1 for step in steps if step["writes"]),
            "negativeStepCount": sum(1 for step in steps if not step["writes"]),
            "minCommitsPerRepo": min(counts),
            "maxCommitsPerRepo": max(counts),
        },
        "pairs": pairs,
        "basenameCollisions": basename_collisions,
        "origins": {"included": with_origins, "relativeRoot": "origins" if with_origins else "", "repos": origins},
        "repos": repos,
        "steps": steps,
    }
    _assert_portable(manifest, "")
    manifest["datasetHash"] = canonical_dataset_hash(manifest)
    return manifest


def write_manifest(root: Path, manifest: Mapping[str, Any]) -> None:
    text = json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    (root / "manifest.json").write_text(text, encoding="utf-8")


def generate(output_dir: str, *, with_origins: bool = True, log_path: str | None = None) -> dict[str, Any]:
    absolute, status = validate_target(output_dir)
    if not git_available():
        raise GitMissingError("git is required to generate the collaboration fixture")
    created = create_target(absolute, status)
    try:
        with GitSession(log_path) as git:
            manifest = build_manifest(git, absolute, with_origins=with_origins)
            write_manifest(absolute, manifest)
            git.log(
                "SUMMARY "
                f"repos={manifest['summary']['repoCount']} "
                f"origins={manifest['summary']['originCount']} "
                f"commits={manifest['summary']['commitCount']} "
                f"refs={manifest['summary']['refCount']} "
                f"steps={len(manifest['steps'])} "
                f"commands={git.commands} "
                f"datasetHash={manifest['datasetHash']}"
            )
        return manifest
    except BaseException:
        if created:
            discard_created(absolute)
        raise


def load_manifest(fixture_dir: str) -> tuple[Path, dict[str, Any]]:
    root = Path(os.path.abspath(fixture_dir))
    if root.is_symlink():
        raise FixtureSafetyError("fixture path is a symlink")
    path = root / "manifest.json"
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("schemaVersion") != SCHEMA_VERSION:
        raise VerificationError("unsupported manifest schemaVersion")
    if manifest.get("kind") != FIXTURE_KIND or manifest.get("standardBenchmark") is not False:
        raise VerificationError("manifest is not the functional acceptance fixture")
    return root, manifest


def _repo_path(root: Path, repo: Mapping[str, Any]) -> Path:
    rel = Path(str(repo["relativePath"]))
    if rel.is_absolute() or ".." in rel.parts:
        raise VerificationError("repo path is not a relative fixture path")
    return root / rel


def verify(fixture_dir: str, *, log_path: str | None = None) -> dict[str, Any]:
    if not git_available():
        raise GitMissingError("git is required to verify the collaboration fixture")
    root, manifest = load_manifest(fixture_dir)
    problems: list[str] = []
    hashed = canonical_dataset_hash(manifest)
    if hashed != manifest.get("datasetHash"):
        problems.append("datasetHash does not match canonical manifest bytes")
    try:
        _assert_portable(manifest, str(root))
    except VerificationError as exc:
        problems.append(str(exc))
    with GitSession(log_path) as git:
        live_repos = []
        for repo in manifest["repos"]:
            path = _repo_path(root, repo)
            try:
                live = load_repo(git, path, repo)
            except (VerificationError, GitCommandError, OSError) as exc:
                problems.append(f"{repo['repoId']}: {exc}")
                continue
            live_repos.append(live)
            if live != repo:
                problems.append(f"{repo['repoId']}: live git/bytes differ from manifest")
            if repo["remoteUrl"]:
                url = git.stdout(path, ["remote", "get-url", "origin"]).strip()
                if url != repo["remoteUrl"] or url.startswith("/") or "://" in url:
                    problems.append(f"{repo['repoId']}: origin url is not the relative local path")
        if manifest["origins"]["included"]:
            for origin in manifest["origins"]["repos"]:
                path = root / origin["relativePath"]
                bare = git.stdout(path, ["rev-parse", "--is-bare-repository"]).strip()
                head = git.rev_parse(path, "refs/heads/main")
                if bare != "true" or head != origin["headOid"]:
                    problems.append(f"{origin['relativePath']}: bare origin mismatch")
                if str(origin["relativePath"]).startswith("machine-"):
                    problems.append("origin is inside a machine workspace")
        if len(manifest["repos"]) != 30:
            problems.append(f"expected 30 repos, found {len(manifest['repos'])}")
        if problems:
            raise VerificationError("; ".join(problems[:12]))
        by_live = {item["repoId"]: item for item in live_repos}
        for step in manifest["steps"]:
            if step["kind"] == "positive-commit":
                source = by_live[step["sourceRepoId"]]
                dest = by_live[step["destRepoId"]]
                selection = select_range(git, _repo_path(root, source), step["selection"]["baseRev"], step["selection"]["tipRev"]) if "baseRev" in step["selection"] else select_last(
                    git,
                    _repo_path(root, source),
                    step["selection"]["tipRev"],
                    int(step["selection"]["count"]),
                )
                if selection["oids"] != step["selection"]["oids"] or not selection["legal"]:
                    problems.append(f"{step['id']}: selection is not the legal git range")
                    continue
                replayed = replay_commits(
                    git,
                    _repo_path(root, source),
                    _repo_path(root, dest),
                    selection["oids"],
                )
                problems.extend(
                    _commit_oracle_problems(git, _repo_path(root, source), step, replayed)
                )
            elif step["kind"] == "positive-file":
                source = by_live[step["sourceRepoId"]]
                dest = by_live[step["destRepoId"]]
                src = _repo_path(root, source)
                for op in step["operations"]:
                    if op["op"] == "delete":
                        try:
                            got = live_deletion(
                                git,
                                src,
                                op["source"]["path"],
                                op["source"]["kind"],
                            )
                        except VerificationError as exc:
                            problems.append(f"{step['id']}: {exc}")
                            continue
                        want = {key: value for key, value in op["source"].items() if key != "repoId"}
                        if got != want or op["dest"]["path"] != op["source"]["path"]:
                            problems.append(f"{step['id']}: source deletion provenance mismatch")
                        if op["dest"]["path"] not in {item["path"] for item in dest["files"]}:
                            problems.append(f"{step['id']}: delete target is already absent")
                        continue
                    kind = op["source"]["kind"]
                    if kind == "working":
                        data = read_nofollow(src / op["source"]["path"])
                    elif kind == "index":
                        _oid, data = _blob_at(git, src, f":{op['source']['path']}")
                    else:
                        _oid, data = _blob_at(git, src, f"{op['source']['rev']}:{op['source']['path']}")
                    if _b64(data) != op["expectedBase64"] or _sha256(data) != op["expectedSha256"]:
                        problems.append(f"{step['id']}: source bytes differ from expected destination bytes")
                    if op["source"]["path"] != op["dest"]["path"] or not op.get("overwrite"):
                        problems.append(f"{step['id']}: file step renames or does not overwrite")
                    dest_row = next(
                        (item for item in dest["files"] if item["path"] == op["dest"]["path"]),
                        None,
                    )
                    if dest_row is None or dest_row["sha256"] != op["destBaselineSha256"]:
                        problems.append(f"{step['id']}: dest overwrite baseline mismatch")
                    if dest_row is not None and dest_row["sha256"] == op["expectedSha256"]:
                        problems.append(f"{step['id']}: overwrite does not change dest bytes")
            elif step["kind"] == "negative" and step["reason"] == "discontinuous":
                recorded = step["selection"]
                fresh = select_range(
                    git,
                    root / "machine-a/west/billing",
                    recorded["baseRev"],
                    recorded["tipRev"],
                )
                if fresh["legal"] or fresh["oldestFirstParent"] == fresh["baseOid"]:
                    problems.append("discontinuous selection is legal against live git")
    if problems:
        raise VerificationError("; ".join(problems[:12]))
    return {
        "datasetHash": manifest["datasetHash"],
        "repos": manifest["summary"]["repoCount"],
        "commits": manifest["summary"]["commitCount"],
        "steps": len(manifest["steps"]),
    }


def _commit_oracle_problems(
    git: GitSession,
    source_repo: Path,
    step: Mapping[str, Any],
    replayed: Mapping[str, Any],
) -> list[str]:
    """Compare manifest expectations to source cat-file identity and a fresh replay.

    New commit OIDs and committer wall time are not in the oracle, so they are
    not compared. Message bytes keep the commit object's trailing newline.
    """
    found: list[str] = []
    step_id = str(step["id"])
    for index, want in enumerate(step["expectedCommits"]):
        parsed = parse_commit_object(git.blob(source_repo, ["cat-file", "commit", want["sourceOid"]]))
        identity = {
            "authorName": parsed["author"]["name"],
            "authorEmail": parsed["author"]["email"],
            "authorTime": parsed["author"]["iso"],
            "message": parsed["message"],
        }
        for field, actual in identity.items():
            if want.get(field) != actual:
                found.append(f"{step_id}: expectedCommits[{index}].{field} != source commit")
    if replayed["commits"] != step["expectedCommits"]:
        found.append(f"{step_id}: expectedCommits != fresh replay")
    if replayed["indexEntries"] != step["expectedIndexEntries"]:
        found.append(f"{step_id}: expectedIndexEntries != fresh replay")
    if replayed["files"] != step["expectedFiles"]:
        found.append(f"{step_id}: expectedFiles != fresh replay")
    if replayed["absentWorktreePaths"] != step["expectedAbsentWorktreePaths"]:
        found.append(f"{step_id}: expectedAbsentWorktreePaths != fresh replay")
    if replayed["finalTreeEntries"] != step["expectedFinalTreeEntries"]:
        found.append(f"{step_id}: expectedFinalTreeEntries != fresh replay")
    if replayed["appliedPaths"] != step["appliedPaths"]:
        found.append(f"{step_id}: appliedPaths != fresh replay")
    return found


def snapshot(fixture_dir: str) -> dict[str, Any]:
    root, manifest = load_manifest(fixture_dir)
    if not git_available():
        raise GitMissingError("git is required to snapshot the collaboration fixture")
    with GitSession(None) as git:
        repos = [load_repo(git, _repo_path(root, repo), repo) for repo in manifest["repos"]]
    return {"schemaVersion": SCHEMA_VERSION, "repos": repos}


def _apply_file_delta(baseline: dict[str, Any], step: Mapping[str, Any]) -> dict[str, Any]:
    expected = copy.deepcopy(baseline)
    files = {item["path"]: item for item in expected["files"]}
    absent = set(expected["absentWorktreePaths"])
    for op in step["operations"]:
        path = op["dest"]["path"]
        if op["op"] == "delete":
            files.pop(path, None)
            absent.add(path)
            continue
        files[path] = {
            "path": path,
            "sha256": op["expectedSha256"],
            "size": op["expectedSize"],
            "base64": op["expectedBase64"],
        }
        absent.discard(path)
    expected["files"] = [files[key] for key in sorted(files)]
    expected["absentWorktreePaths"] = sorted(absent)
    return expected


def _new_commits(repo: Mapping[str, Any], baseline_head: str) -> list[dict[str, Any]]:
    by_oid = {item["oid"]: item for item in repo["commits"]}
    current = repo["head"]["oid"]
    found: list[dict[str, Any]] = []
    seen: set[str] = set()
    while current and current != baseline_head:
        if current in seen or current not in by_oid:
            raise CompareError("new commit chain does not lead back to the baseline HEAD")
        seen.add(current)
        item = by_oid[current]
        found.append(item)
        if len(found) > 20:
            raise CompareError("too many new commits")
        parents = item["parents"]
        current = parents[0] if parents else ""
    found.reverse()
    return found


def compare_step(
    fixture_dir: str,
    step_id: str,
    snap: Mapping[str, Any],
    phase: str,
    *,
    baseline: Mapping[str, Any] | None = None,
) -> None:
    _root, manifest = load_manifest(fixture_dir)
    step = next((item for item in manifest["steps"] if item["id"] == step_id), None)
    if step is None:
        raise CompareError(f"unknown step {step_id}")
    if phase not in {"initial", "applied"}:
        raise CompareError(f"unknown phase {phase}")
    base = {item["repoId"]: item for item in (baseline["repos"] if baseline is not None else manifest["repos"])}
    got = {item["repoId"]: item for item in snap["repos"]}
    if set(base) != set(got):
        raise CompareError("snapshot repo ids differ from the fixture")
    if (
        step.get("noWriteSnapshot") == "immediately-before-action-after-setup-diff"
        and phase == "applied"
        and baseline is None
    ):
        raise CompareError(
            f"{step_id} no-write oracle is the snapshot taken immediately before the action "
            "after the setup diff, not the generation baseline"
        )
    no_write = phase == "initial" or not step["writes"]
    if no_write:
        if got != base:
            raise CompareError(f"{step_id} {phase} expected the full baseline snapshot")
        return
    dest_id = step["destRepoId"]
    for repo_id_value, baseline in base.items():
        if repo_id_value != dest_id and got[repo_id_value] != baseline:
            raise CompareError(f"unselected repo changed: {repo_id_value}")
    dest_base = base[dest_id]
    dest_got = got[dest_id]
    if step["kind"] == "positive-file":
        if _apply_file_delta(dest_base, step) != dest_got:
            raise CompareError(f"{step_id} working tree does not match the file plan")
        return
    if step["kind"] != "positive-commit":
        raise CompareError(f"{step_id} has no applied projection")
    if dest_got["head"]["branch"] != step["preserved"]["headBranch"]:
        raise CompareError("HEAD branch name changed")
    created = _new_commits(dest_got, step["preserved"]["baselineHeadOid"])
    expected = step["expectedCommits"]
    if len(created) != len(expected):
        raise CompareError("new commit count mismatch")
    for got_commit, want in zip(created, expected):
        if len(got_commit["parents"]) != 1:
            raise CompareError("replayed commit does not have one parent")
        for field in ("tree", "authorName", "authorEmail", "authorTime", "message"):
            if got_commit[field] != want[field]:
                raise CompareError(f"replay field {field} mismatch")
        if got_commit["oid"] == want["sourceOid"]:
            raise CompareError("replay pinned the source commit OID")
    if dest_got["indexEntries"] != step["expectedIndexEntries"]:
        raise CompareError("index does not match the commit-only oracle")
    if dest_got["files"] != step["expectedFiles"] or dest_got["absentWorktreePaths"] != step["expectedAbsentWorktreePaths"]:
        raise CompareError("worktree bytes do not match the commit oracle")
    baseline_refs = {item["name"]: item for item in dest_base["refs"]}
    got_refs = {item["name"]: item for item in dest_got["refs"]}
    branch = step["preserved"]["headBranch"]
    if set(got_refs) != set(baseline_refs):
        extra = sorted(set(got_refs) - set(baseline_refs))
        missing = sorted(set(baseline_refs) - set(got_refs))
        raise CompareError(f"ref namespace mismatch extra={extra} missing={missing}")
    for name, ref in baseline_refs.items():
        if name == branch:
            continue
        if got_refs[name] != ref:
            raise CompareError(f"ref changed: {name}")
    if branch not in got_refs or got_refs[branch]["oid"] != dest_got["head"]["oid"]:
        raise CompareError("HEAD branch ref does not match HEAD")


def preview_is_stale(preview: Mapping[str, Any], repo: Mapping[str, Any]) -> bool:
    if repo["head"]["oid"] != preview["headOid"] or repo["head"]["branch"] != preview["headBranch"]:
        return True
    if repo["indexEntriesSha256"] != preview["indexEntriesSha256"]:
        return True
    current = next((item["sha256"] for item in repo["files"] if item["path"] == preview["path"]), "")
    return current != preview["contentSha256"]


def _print_summary(manifest: Mapping[str, Any], manifest_path: Path) -> None:
    summary = manifest["summary"]
    print(f"datasetHash={manifest['datasetHash']}")
    print(f"repos={summary['repoCount']} origins={summary['originCount']} commits={summary['commitCount']}")
    print(f"steps={summary['positiveStepCount'] + summary['negativeStepCount']}")
    print(f"manifest={manifest_path}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="collaboration_fixture")
    sub = parser.add_subparsers(dest="cmd", required=True)
    generate_cmd = sub.add_parser("generate")
    generate_cmd.add_argument("--output", required=True)
    generate_cmd.add_argument("--no-origins", action="store_true")
    generate_cmd.add_argument("--log")
    verify_cmd = sub.add_parser("verify")
    verify_cmd.add_argument("--fixture", required=True)
    verify_cmd.add_argument("--log")
    snap_cmd = sub.add_parser("snapshot")
    snap_cmd.add_argument("--fixture", required=True)
    snap_cmd.add_argument("--output", required=True)
    compare_cmd = sub.add_parser("compare-step")
    compare_cmd.add_argument("--fixture", required=True)
    compare_cmd.add_argument("--step", required=True)
    compare_cmd.add_argument("--snapshot", required=True)
    compare_cmd.add_argument("--baseline", "--baseline-snapshot", dest="baseline")
    compare_cmd.add_argument("--phase", required=True, choices=["initial", "applied"])
    args = parser.parse_args(argv)
    try:
        if args.cmd == "generate":
            manifest = generate(args.output, with_origins=not args.no_origins, log_path=args.log)
            _print_summary(manifest, Path(args.output) / "manifest.json")
            return 0
        if args.cmd == "verify":
            result = verify(args.fixture, log_path=args.log)
            print(f"verify ok datasetHash={result['datasetHash']} repos={result['repos']} commits={result['commits']} steps={result['steps']}")
            return 0
        if args.cmd == "snapshot":
            payload = snapshot(args.fixture)
            Path(args.output).write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            print(f"snapshot={args.output} repos={len(payload['repos'])}")
            return 0
        snap = json.loads(Path(args.snapshot).read_text(encoding="utf-8"))
        baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8")) if getattr(args, "baseline", None) else None
        compare_step(args.fixture, args.step, snap, args.phase, baseline=baseline)
        print(f"compare ok step={args.step} phase={args.phase}")
        return 0
    except FixtureSafetyError as exc:
        print(f"safety: {exc}", file=sys.stderr)
        return 2
    except GitMissingError as exc:
        print(f"git-missing: {exc}", file=sys.stderr)
        return 3
    except (VerificationError, CompareError, GitCommandError, OSError, json.JSONDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
