#!/usr/bin/env python3
"""X11/Fcitx5 acceptance for the native workbench Chinese IME.

Runs on a private Xvfb, a private session bus, and a private HOME. It never
touches the caller's DISPLAY or Fcitx config. Geometry is measured from the
root screenshot and from the control bounds the app logs.

Exit codes:
  0  required checks passed, including caret association
  2  required checks failed, or a required tool/display/IME is missing while
     SNIP_REQUIRE_ALL_TESTS=1
  3  required checks passed, but a documented platform limit remains
  0  when tools are missing and SNIP_REQUIRE_ALL_TESTS is unset: prints
     UNSUPPORTED and does not claim the IME was exercised
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import select
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

MARKER = "snip-native-ime-fix-20260926"
RUN = Path("/tmp") / MARKER
KNOWN_PHRASE = "你好"
COMMIT_SUBJECT = "探針片語你好"
DECOY_SUBJECT = "世界別章"
ASCII_SUBJECT = "ascii baseline"
SENTINEL = b"IME_ORACLE_SENTINEL_7f3a"
REQUIRED_TOOLS = (
    "Xvfb",
    "xdotool",
    "xwininfo",
    "xwd",
    "convert",
    "fcitx5",
    "fcitx5-remote",
    "dbus-daemon",
    "xclip",
    "git",
)
BOUNDS_RE = re.compile(
    r"\[APP:CTRL_BOUNDS: id=(?P<id>.+?) x=(?P<x>-?\d+) y=(?P<y>-?\d+) "
    r"w=(?P<w>\d+) h=(?P<h>\d+)\]"
)


class ImeCheckError(Exception):
    pass


def decide_run(missing: list[str], require_all: bool) -> str:
    """run, unsupported, or fail. Missing display/IME fails closed when required."""
    if not missing:
        return "run"
    if require_all:
        return "fail"
    return "unsupported"


def missing_tools(names: tuple[str, ...] = REQUIRED_TOOLS) -> list[str]:
    return [name for name in names if shutil.which(name) is None]


def codepoints(text: str | None) -> list[str] | None:
    if text is None:
        return None
    return [f"U+{ord(ch):04X}" for ch in text]


def parse_bounds(lines: list[str]) -> dict[str, tuple[int, int, int, int]]:
    out: dict[str, tuple[int, int, int, int]] = {}
    for line in lines:
        match = BOUNDS_RE.search(line)
        if match:
            out[match["id"]] = (
                int(match["x"]),
                int(match["y"]),
                int(match["w"]),
                int(match["h"]),
            )
            continue
        gone = re.search(r"\[APP:CTRL_GONE: id=(.+?)\]", line)
        if gone:
            out.pop(gone[1], None)
    return out


def parse_xwininfo(text: str) -> dict[str, int | str]:
    def field(label: str) -> str:
        match = re.search(rf"^\s*{re.escape(label)}:\s*(.+)$", text, re.M)
        if not match:
            raise ImeCheckError(f"xwininfo output lacks {label!r}")
        return match[1].strip()

    return {
        "x": int(field("Absolute upper-left X")),
        "y": int(field("Absolute upper-left Y")),
        "width": int(field("Width")),
        "height": int(field("Height")),
        "mapState": field("Map State"),
    }


def rect(x: int, y: int, w: int, h: int) -> dict[str, int]:
    return {"x": x, "y": y, "w": w, "h": h}


def abs_bounds(
    window: dict, bounds: tuple[int, int, int, int] | None
) -> dict[str, int] | None:
    if bounds is None:
        return None
    x, y, w, h = bounds
    return rect(int(window["x"]) + x, int(window["y"]) + y, w, h)


def associated_candidate(
    popup: dict[str, int], field: dict[str, int], window: dict[str, int]
) -> dict:
    """Whether a screenshot popup sits on the field caret, not the window frame.

    `popup`, `field`, and `window` are screen-pixel rectangles. The caret is
    inside `field`; Fcitx's unset-spot fallback puts the popup just below the
    window's bottom edge.
    """
    field_bottom = field["y"] + field["h"]
    window_bottom = window["y"] + window["h"]
    pop_right = popup["x"] + popup["w"]
    field_left = field["x"] - 48
    field_right = field["x"] + field["w"] + 48
    horizontal = popup["x"] < field_right and pop_right > field_left
    vertical = (field["y"] - 12) <= popup["y"] <= (field_bottom + 80)
    below_window = popup["y"] >= window_bottom - 2
    field_at_window_bottom = field_bottom >= window_bottom - 8
    window_bottom_fallback = below_window and not field_at_window_bottom
    anchored = horizontal and vertical and not window_bottom_fallback
    return {
        "anchored": anchored,
        "horizontalOverlap": horizontal,
        "verticalNearField": vertical,
        "windowBottomFallback": window_bottom_fallback,
        "dyFromFieldBottom": popup["y"] - field_bottom,
        "dxFromFieldLeft": popup["x"] - field["x"],
        "dyFromWindowBottom": popup["y"] - window_bottom,
    }


def bright_popups(image) -> list[dict[str, int]]:
    """Near-white blobs large enough to be an Fcitx classic candidate bar."""
    rgb = image.convert("RGB")
    width, height = rgb.size
    pixels = rgb.load()
    bands: list[tuple[int, int, int, int]] = []
    row = 0
    while row < height:
        xs = [
            x
            for x in range(width)
            if _near_white(pixels[x, row])
        ]
        if len(xs) < 40:
            row += 1
            continue
        y0 = row
        min_x, max_x = min(xs), max(xs)
        row += 1
        while row < height:
            xs = [
                x
                for x in range(width)
                if _near_white(pixels[x, row])
            ]
            if len(xs) < 40:
                break
            min_x = min(min_x, min(xs))
            max_x = max(max_x, max(xs))
            row += 1
        y1 = row
        w, h = max_x - min_x + 1, y1 - y0
        if w >= 80 and 18 <= h <= 140:
            bands.append((min_x, y0, w, h))
    bands.sort(key=lambda item: item[2] * item[3], reverse=True)
    return [rect(x, y, w, h) for x, y, w, h in bands]


def _near_white(pixel: tuple[int, int, int]) -> bool:
    r, g, b = pixel
    return min(r, g, b) >= 235 and max(r, g, b) - min(r, g, b) <= 18


def shutdown_graceful(steps: list[dict]) -> bool:
    """True only when every owned process exited without SIGKILL."""
    return bool(steps) and all(step.get("result") != "sigkill" for step in steps)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def proc_starttime(pid: int) -> int | None:
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    except OSError:
        return None
    # starttime is field 22 (1-based) of the post-comm stat, index 19.
    try:
        return int(fields[19])
    except (IndexError, ValueError):
        return None


def same_process(pid: int, starttime: int | None) -> bool:
    return starttime is not None and proc_starttime(pid) == starttime


def cmdline(pid: int) -> str:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return ""
    return raw.replace(b"\0", b" ").decode("utf-8", "replace").strip()


def proc_environ(pid: int) -> dict[str, str]:
    try:
        raw = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError:
        return {}
    out = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            key, value = item.split(b"=", 1)
            out[key.decode("utf-8", "replace")] = value.decode("utf-8", "replace")
    return out


def pids_with_comm(name: str) -> list[int]:
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            comm = Path(f"/proc/{entry}/comm").read_text().strip()
        except OSError:
            continue
        if comm == name:
            found.append(int(entry))
    return found


def lavapipe_icd() -> str:
    machine = os.uname().machine
    roots = ("/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d")
    names = (f"lvp_icd.{machine}.json", "lvp_icd.json")
    for root in roots:
        for name in names:
            path = os.path.join(root, name)
            if os.path.isfile(path):
                return path
        if os.path.isdir(root):
            for name in sorted(os.listdir(root)):
                if name.startswith("lvp_icd") and name.endswith(".json"):
                    return os.path.join(root, name)
    raise ImeCheckError("no lavapipe ICD (install mesa-vulkan-drivers)")


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
        env.pop(key, None)
    env["GIT_CONFIG_GLOBAL"] = "/dev/null"
    env["GIT_CONFIG_SYSTEM"] = "/dev/null"
    env["GIT_AUTHOR_NAME"] = "Probe"
    env["GIT_AUTHOR_EMAIL"] = "probe@example.invalid"
    env["GIT_COMMITTER_NAME"] = "Probe"
    env["GIT_COMMITTER_EMAIL"] = "probe@example.invalid"
    return env


def main(argv: list[str] | None = None) -> int:
    args = argv if argv is not None else sys.argv[1:]
    require_all = "SNIP_REQUIRE_ALL_TESTS" in os.environ
    missing = missing_tools()
    decision = decide_run(missing, require_all)
    if decision == "unsupported":
        print(
            "UNSUPPORTED: native IME check was not run "
            f"(missing {', '.join(missing)}). "
            "Set SNIP_REQUIRE_ALL_TESTS=1 to fail closed."
        )
        return 0
    if decision == "fail":
        print(
            "FAIL: native IME check requires "
            f"{', '.join(missing)} (SNIP_REQUIRE_ALL_TESTS=1)",
            file=sys.stderr,
        )
        return 2
    binary = _binary_arg(args)
    if binary is None or not binary.is_file() or not os.access(binary, os.X_OK):
        print(f"FAIL: native binary missing or not executable: {binary}", file=sys.stderr)
        return 2
    try:
        return _run(binary)
    except ImeCheckError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 2


def _binary_arg(args: list[str]) -> Path | None:
    if "--binary" in args:
        index = args.index("--binary")
        if index + 1 >= len(args):
            return None
        return Path(args[index + 1])
    env = os.environ.get("SNIP_NATIVE_IME_BIN")
    return Path(env) if env else None


def _run(binary: Path) -> int:
    if RUN.exists():
        shutil.rmtree(RUN)
    RUN.mkdir(parents=True)
    result: dict = {
        "kind": "x11-fcitx5-native-ime-check",
        "binary": {"path": str(binary), "sha256": sha256_file(binary)},
        "phases": [],
        "screenshots": [],
        "platformLimits": [],
        "shutdown": [],
    }
    session = Session()
    preexisting = {
        name: pids_with_comm(name) for name in ("Xvfb", "fcitx5", "dbus-daemon")
    }
    user_profile = Path.home() / ".config/fcitx5/profile"
    result["userFcitxProfileBefore"] = _profile_sig(user_profile)
    result["preexistingPids"] = preexisting
    exit_code = 2
    try:
        result["displayTools"] = {
            "xdpyinfo": shutil.which("xdpyinfo") is not None,
        }
        _prepare_repo(session)
        session.prepare_env()
        session.start_xvfb()
        if session.display in {":1", os.environ.get("DISPLAY")}:
            raise ImeCheckError(f"refusing shared display {session.display}")
        session.start_dbus()
        session.start_fcitx()
        result["display"] = session.display
        result["scale"] = session.scale_evidence()
        session.start_app(binary)
        exit_code = _exercise(session, result)
    except ImeCheckError as exc:
        result["error"] = str(exc)
        exit_code = 2
    finally:
        result["shutdown"] = session.stop()
        result["imeTraces"] = session.traces(0)
        result["graceful"] = shutdown_graceful(result["shutdown"])
        result["userFcitxProfileAfter"] = _profile_sig(user_profile)
        result["userFcitxProfileUnchanged"] = (
            result["userFcitxProfileBefore"] == result["userFcitxProfileAfter"]
        )
        result["preexistingStillAlive"] = {
            name: [pid for pid in pids if same_process(pid, proc_starttime(pid))]
            for name, pids in preexisting.items()
        }
        result["exitCode"] = exit_code
        (RUN / "result.json").write_text(
            json.dumps(result, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"result {RUN / 'result.json'} exit {exit_code} graceful {result['graceful']}")
    return exit_code


def _profile_sig(path: Path) -> list[int] | None:
    if not path.exists():
        return None
    stat = path.stat()
    return [stat.st_mtime_ns, stat.st_size]


def _prepare_repo(session: "Session") -> None:
    repo = session.repo
    if repo.exists():
        shutil.rmtree(repo)
    repo.mkdir(parents=True)
    env = git_env()

    def run(*args: str) -> None:
        res = subprocess.run(
            ["git", "-C", str(repo), *args], env=env, capture_output=True, text=True
        )
        if res.returncode != 0:
            raise ImeCheckError(f"git {args}: {res.stderr[:300]}")

    run("init", "-b", "main")
    run("config", "user.name", "Probe")
    run("config", "user.email", "probe@example.invalid")
    run("config", "commit.gpgsign", "false")
    for index, subject in enumerate((ASCII_SUBJECT, COMMIT_SUBJECT, DECOY_SUBJECT), start=1):
        (repo / "a.txt").write_text(f"rev {index} {subject}\n", encoding="utf-8")
        message = RUN / f"commit-msg-{index}.txt"
        message.write_text(subject + "\n", encoding="utf-8")
        run("add", "a.txt")
        run("commit", "-F", str(message))


class Session:
    def __init__(self) -> None:
        self.run = RUN
        self.iso = RUN / "iso"
        self.repo = RUN / "workspace" / "ime-repo"
        self.env: dict[str, str] = {}
        self.display = ""
        self.app: subprocess.Popen | None = None
        self.xvfb: subprocess.Popen | None = None
        self.dbus: subprocess.Popen | None = None
        self.fcitx: subprocess.Popen | None = None
        self.lines: list[str] = []
        self.stderr_lines: list[str] = []
        self.owned: list[tuple[int, int | None, str]] = []
        self._files: list = []
        self._lock = threading.Lock()
        self._reader: threading.Thread | None = None
        self._err_reader: threading.Thread | None = None

    def remember(self, pid: int, label: str) -> None:
        start = proc_starttime(pid)
        if start is None:
            return
        if any(item[0] == pid and item[1] == start for item in self.owned):
            return
        self.owned.append((pid, start, label))

    def prepare_env(self) -> None:
        self.iso.mkdir()
        home = self.iso / "home"
        home.mkdir()
        runtime = self.iso / "runtime"
        runtime.mkdir(mode=0o700)
        os.chmod(runtime, 0o700)
        config = self.iso / "config"
        data = self.iso / "data"
        cache = self.iso / "cache"
        for path in (config, data, cache):
            path.mkdir()
        bus = self.iso / "session-bus.xml"
        # D-Bus owns a unique short socket; evidence paths can exceed AF_UNIX limits.
        bus.write_text(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" "
            "\"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n"
            "<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen>"
            "<auth>EXTERNAL</auth><policy context=\"default\">"
            "<allow send_destination=\"*\" eavesdrop=\"true\"/>"
            "<allow eavesdrop=\"true\"/><allow receive_sender=\"*\"/>"
            "<allow own=\"*\"/></policy></busconfig>\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        for key in (
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SESSION_BUS_PID",
            "GTK_IM_MODULE",
            "QT_IM_MODULE",
            "IBUS_ADDRESS",
        ):
            env.pop(key, None)
        env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(config),
                "XDG_DATA_HOME": str(data),
                "XDG_CACHE_HOME": str(cache),
                "XDG_RUNTIME_DIR": str(runtime),
                "XMODIFIERS": "@im=fcitx",
                "SNIP_IME_MARKER": MARKER,
                "SNIP_NATIVE_E2E": "1",
                # bright_popups() finds the near-white candidate bar on the dark palette.
                "SNIP_THEME": "dark",
                "SNIP_IME_TRACE": "1",
                "VK_DRIVER_FILES": lavapipe_icd(),
                "LIBGL_ALWAYS_SOFTWARE": "1",
                "GALLIUM_DRIVER": "llvmpipe",
            }
        )
        self.env = env
        self._bus_config = str(bus)

    def start_xvfb(self) -> None:
        log = open(self.run / "xvfb.log", "wb")
        self._files.append(log)
        read_fd, write_fd = os.pipe()
        self.xvfb = subprocess.Popen(
            [
                "Xvfb",
                "-displayfd",
                str(write_fd),
                "-screen",
                "0",
                "1280x900x24",
                "-nolisten",
                "tcp",
                "-noreset",
                "-ac",
            ],
            pass_fds=(write_fd,),
            stdout=log,
            stderr=log,
            start_new_session=True,
            close_fds=True,
        )
        os.close(write_fd)
        ready, _, _ = select.select([read_fd], [], [], 15)
        display = os.read(read_fd, 64).decode().strip() if ready else ""
        os.close(read_fd)
        if not display.isdigit() or display == "1":
            raise ImeCheckError(f"Xvfb displayfd returned {display!r}")
        self.display = f":{display}"
        self.env["DISPLAY"] = self.display
        self.remember(self.xvfb.pid, "Xvfb")
        if not _wait(lambda: _display_owned(self.xvfb.pid, display), 5):
            raise ImeCheckError("Xvfb does not own its display socket")

    def start_dbus(self) -> None:
        log = open(self.run / "dbus.log", "wb")
        self._files.append(log)
        read_addr, write_addr = os.pipe()
        self.dbus = subprocess.Popen(
            [
                "dbus-daemon",
                "--config-file",
                self._bus_config,
                "--nofork",
                f"--print-address={write_addr}",
                "--nopidfile",
            ],
            pass_fds=(write_addr,),
            stdout=log,
            stderr=log,
            start_new_session=True,
            close_fds=True,
        )
        os.close(write_addr)
        address = _read_line(read_addr, 5)
        os.close(read_addr)
        if not address.startswith("unix:"):
            raise ImeCheckError(f"dbus address unusable: {address!r}")
        self.env["DBUS_SESSION_BUS_ADDRESS"] = address
        self.remember(self.dbus.pid, "dbus-daemon")

    def start_fcitx(self) -> None:
        conf = Path(self.env["XDG_CONFIG_HOME"]) / "fcitx5"
        (conf / "conf").mkdir(parents=True)
        (conf / "profile").write_text(
            "[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=pinyin\n\n"
            "[Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n"
            "[Groups/0/Items/1]\nName=pinyin\nLayout=\n\n"
            "[GroupOrder]\n0=Default\n",
            encoding="utf-8",
        )
        # Default UseOnTheSpot is false. Do not enable it: that is a different
        # Fcitx mode from the verified failure.
        (conf / "conf" / "classicui.conf").write_text(
            "[ClassicUI]\nVertical Candidate List=False\nPerScreenDPI=False\n",
            encoding="utf-8",
        )
        log = open(self.run / "fcitx.log", "wb")
        self._files.append(log)
        self.fcitx = subprocess.Popen(
            [
                "fcitx5",
                "-D",
                "--keep",
                "-u",
                "classicui",
                "--disable",
                "wayland,waylandim,kimpanel,notificationitem,notifications,cloudpinyin,ibusfrontend",
                "--verbose",
                "default=3,key_trace=5,xim=5",
            ],
            env=self.env,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        self.remember(self.fcitx.pid, "fcitx5")
        end = time.monotonic() + 20
        while time.monotonic() < end:
            if self.fcitx.poll() is not None:
                raise ImeCheckError(f"fcitx5 exited {self.fcitx.returncode}")
            state = subprocess.run(
                ["fcitx5-remote"], env=self.env, capture_output=True, text=True
            )
            atoms = self.x_text("xlsatoms", check=False)
            if state.stdout.strip() in {"0", "1", "2"} and "@server=fcitx" in atoms:
                return
            time.sleep(0.25)
        raise ImeCheckError("fcitx5/XIM did not become ready")

    def scale_evidence(self) -> dict:
        info = self.x_text("xdpyinfo", check=False)
        dimensions = ""
        dpi = ""
        for line in info.splitlines():
            if "dimensions:" in line:
                dimensions = line.strip()
            if "resolution:" in line:
                dpi = line.strip()
        return {"dimensions": dimensions, "resolution": dpi, "display": self.display}

    def start_app(self, binary: Path) -> None:
        bin_dir = self.run / "bin"
        bin_dir.mkdir()
        wrapper = bin_dir / "git"
        wrapper.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "log = os.environ.get('SNIP_GIT_ARGV_LOG')\n"
            "if log:\n"
            "    with open(log, 'a', encoding='utf-8') as fh:\n"
            "        fh.write(json.dumps(sys.argv[1:], ensure_ascii=False) + '\\n')\n"
            "os.execv('/usr/bin/git', ['/usr/bin/git', *sys.argv[1:]])\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        self.env["PATH"] = str(bin_dir) + os.pathsep + self.env.get("PATH", "")
        self.env["SNIP_GIT_ARGV_LOG"] = str(self.run / "git-argv.log")
        stdout = open(self.run / "app.log", "wb")
        stderr = open(self.run / "app.err", "wb")
        self._files.extend((stdout, stderr))
        self.app = subprocess.Popen(
            [
                str(binary),
                "--workspace",
                str(self.repo.parent),
                "--mode",
                "normal",
            ],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.remember(self.app.pid, "snip-desktop-native")

        def pump(pipe, bucket, sink):
            assert pipe is not None
            for raw in pipe:
                text = raw.decode("utf-8", "replace").rstrip("\n")
                with self._lock:
                    bucket.append(text)
                sink.write((text + "\n").encode())
                sink.flush()

        assert self.app.stdout is not None and self.app.stderr is not None
        self._reader = threading.Thread(
            target=pump, args=(self.app.stdout, self.lines, stdout), daemon=True
        )
        self._err_reader = threading.Thread(
            target=pump, args=(self.app.stderr, self.stderr_lines, stderr), daemon=True
        )
        self._reader.start()
        self._err_reader.start()
        self.wait_line(lambda line: "[APP:WINDOW_READY]" in line, timeout=40)
        self.wait_line(lambda line: "[APP:REPO_LOADED:" in line, timeout=30)
        self.wait_line(lambda line: "[APP:GRAPH_LOADED:" in line, timeout=20)
        self.wait_line(lambda line: "id=log-search-input " in line, timeout=15)

    def x(self, *args: str, timeout: float = 25, check: bool = True) -> subprocess.CompletedProcess:
        display = self.env.get("DISPLAY")
        if display in (None, "", ":1", os.environ.get("DISPLAY")):
            raise ImeCheckError(f"refusing X command on {display!r}")
        res = subprocess.run(
            list(args), env=self.env, capture_output=True, timeout=timeout
        )
        if check and res.returncode != 0:
            err = res.stderr.decode("utf-8", "replace")[:300]
            raise ImeCheckError(f"{' '.join(args)} exited {res.returncode}: {err}")
        return res

    def x_text(self, *args: str, timeout: float = 25, check: bool = True) -> str:
        return self.x(*args, timeout=timeout, check=check).stdout.decode(
            "utf-8", "replace"
        )

    def texts(self, start: int = 0) -> list[str]:
        with self._lock:
            return self.lines[start:]

    def traces(self, start: int = 0) -> list[str]:
        with self._lock:
            return [
                line
                for line in self.stderr_lines[start:]
                if line.startswith("[IME_TRACE]")
            ]

    def wait_line(self, pred, start: int = 0, timeout: float = 20):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for line in self.texts(start):
                if pred(line):
                    return line
            if self.app is not None and self.app.poll() is not None:
                raise ImeCheckError(f"app exited {self.app.returncode} while waiting")
            time.sleep(0.05)
        raise ImeCheckError("timed out waiting for an app log line")

    def window(self) -> dict:
        assert self.app is not None
        end = time.monotonic() + 20
        while time.monotonic() < end:
            res = subprocess.run(
                ["xdotool", "search", "--pid", str(self.app.pid)],
                env=self.env,
                capture_output=True,
                text=True,
            )
            for wid in res.stdout.split():
                geo = parse_xwininfo(self.x_text("xwininfo", "-id", wid))
                if geo["mapState"] == "IsViewable" and int(geo["width"]) > 1:
                    return {"wid": wid, **geo}
            time.sleep(0.1)
        raise ImeCheckError("no viewable window for the app")

    def refresh(self, win: dict) -> dict:
        geo = parse_xwininfo(self.x_text("xwininfo", "-id", win["wid"]))
        return {"wid": win["wid"], **geo}

    def focus(self, wid: str) -> None:
        self.x("xdotool", "windowfocus", "--sync", wid)
        focused = self.x_text("xdotool", "getwindowfocus").strip()
        if focused != wid:
            raise ImeCheckError(f"X focus is {focused}, not {wid}")

    def key(self, wid: str, *keys: str, delay: int = 90) -> None:
        self.focus(wid)
        time.sleep(0.05)
        self.x("xdotool", "key", "--clearmodifiers", "--delay", str(delay), *keys)
        self.x("xdotool", "keyup", "ctrl", "alt", "shift", "super")

    def click_bounds(self, win: dict, bounds: tuple[int, int, int, int]) -> dict[str, int]:
        x, y, w, h = bounds
        ax, ay = int(win["x"]) + x + w // 2, int(win["y"]) + y + h // 2
        self.focus(win["wid"])
        time.sleep(0.05)
        self.x("xdotool", "mousemove", "--sync", str(ax), str(ay), "click", "1")
        return {"x": ax, "y": ay}

    def screenshot(self, win: dict, name: str) -> dict:
        win = self.refresh(win)
        root_png = self.run / f"{name}-root.png"
        window_png = self.run / f"{name}-window.png"
        xwd = self.iso / f"{name}.xwd"
        self.x("xwd", "-root", "-silent", "-out", str(xwd))
        self.x("convert", str(xwd), str(root_png))
        self.x(
            "convert",
            str(root_png),
            "-crop",
            f"{win['width']}x{win['height']}+{win['x']}+{win['y']}",
            "+repage",
            str(window_png),
        )
        xwd.unlink(missing_ok=True)
        return {
            "root": str(root_png),
            "window": str(window_png),
            "geometry": {k: win[k] for k in ("x", "y", "width", "height", "wid")},
        }

    def stop(self) -> list[dict]:
        steps: list[dict] = []
        if self.app is not None and self.app.poll() is None and self.display:
            try:
                win = self.window()
                self.key(win["wid"], "ctrl+q")
            except Exception as exc:  # noqa: BLE001
                steps.append({"label": "app-ctrl-q", "result": f"error:{exc}"})
            if _wait(lambda: self.app.poll() is not None, 5):
                steps.append(
                    {
                        "label": "snip-desktop-native",
                        "pid": self.app.pid,
                        "result": "exited",
                        "code": self.app.returncode,
                    }
                )
        if self.fcitx is not None and self.fcitx.poll() is None:
            subprocess.run(
                ["fcitx5-remote", "-e"],
                env=self.env,
                capture_output=True,
                timeout=5,
            )
            if _wait(lambda: self.fcitx.poll() is not None, 8):
                steps.append(
                    {
                        "label": "fcitx5",
                        "pid": self.fcitx.pid,
                        "result": "exited",
                        "code": self.fcitx.returncode,
                    }
                )
        for proc, label in (
            (self.app, "snip-desktop-native"),
            (self.fcitx, "fcitx5"),
            (self.dbus, "dbus-daemon"),
            (self.xvfb, "Xvfb"),
        ):
            if proc is None:
                continue
            self.remember(proc.pid, label)
            steps.extend(_stop_group(proc, label))
        for pid, start, label in list(self.owned):
            if same_process(pid, start):
                steps.extend(_signal_pid(pid, start, label))
        if self._reader is not None:
            self._reader.join(timeout=2)
        if self._err_reader is not None:
            self._err_reader.join(timeout=2)
        if self.app is not None:
            if self.app.stdout is not None:
                self.app.stdout.close()
            if self.app.stderr is not None:
                self.app.stderr.close()
        for handle in self._files:
            handle.close()
        self._files.clear()
        return steps


def _stop_group(proc: subprocess.Popen, label: str) -> list[dict]:
    if proc.poll() is not None:
        return [{"label": label, "pid": proc.pid, "result": "exited", "code": proc.returncode}]
    try:
        pgid = os.getpgid(proc.pid)
    except ProcessLookupError:
        return [{"label": label, "pid": proc.pid, "result": "exited"}]
    try:
        os.kill(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return [{"label": label, "pid": proc.pid, "result": "exited"}]
    except OSError as exc:
        return [{"label": label, "pid": proc.pid, "result": f"sigterm-error:{exc}"}]
    # Reap our child. A zombie still has its original starttime, so a
    # /proc starttime check would treat an already-exited process as alive.
    if _wait(lambda: proc.poll() is not None, 8):
        return [
            {
                "label": label,
                "pid": proc.pid,
                "result": "exited-after-sigterm",
                "code": proc.returncode,
            }
        ]
    try:
        os.kill(proc.pid, signal.SIGKILL)
    except OSError:
        pass
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    return [{"label": label, "pid": proc.pid, "result": "sigkill", "pgid": pgid}]


def _signal_pid(pid: int, start: int | None, label: str) -> list[dict]:
    if not same_process(pid, start):
        return []
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        return []
    if _wait(lambda: not same_process(pid, start), 2):
        return [{"label": label, "pid": pid, "result": "exited-after-sigterm"}]
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    return [{"label": label, "pid": pid, "result": "sigkill"}]


def _display_owned(pid: int, number: str) -> bool:
    inodes = set()
    fd_dir = Path(f"/proc/{pid}/fd")
    if not fd_dir.is_dir():
        return False
    for fd in fd_dir.iterdir():
        try:
            target = os.readlink(fd)
        except OSError:
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            inodes.add(target[len("socket:[") : -1])
    needle = f"/tmp/.X11-unix/X{number}"
    try:
        lines = Path("/proc/net/unix").read_text(errors="replace").splitlines()
    except OSError:
        return False
    for line in lines[1:]:
        parts = line.split()
        if len(parts) >= 8 and parts[6] in inodes and parts[-1] == needle:
            return True
    return False


def _read_line(fd: int, timeout: float) -> str:
    buf = b""
    end = time.monotonic() + timeout
    while b"\n" not in buf and time.monotonic() < end:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if not ready:
            continue
        chunk = os.read(fd, 512)
        if not chunk:
            break
        buf += chunk
    return buf.split(b"\n", 1)[0].decode("utf-8", "replace").strip()


def _wait(pred, timeout: float) -> bool:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if pred():
            return True
        time.sleep(0.05)
    return False


def _settle(take, ok, timeout: float = 5.0):
    """Retry take() until ok(value) or timeout; return the last value."""
    end = time.monotonic() + timeout
    while True:
        value = take()
        if ok(value) or time.monotonic() >= end:
            return value
        time.sleep(0.1)


def _exercise(session: Session, result: dict) -> int:
    failures: list[str] = []
    win = session.window()
    result["initialWindow"] = {k: win[k] for k in ("x", "y", "width", "height")}
    viewport = _viewport(session.texts())
    result["initialViewport"] = viewport
    shot = session.screenshot(win, "00-ready")
    result["screenshots"].append({"name": "00-ready", **shot})
    ready_popup = _popup_at(shot["root"])
    result["readyPopup"] = ready_popup
    if ready_popup is not None:
        failures.append(f"white popup present before composition: {ready_popup}")
    controls = parse_bounds(session.texts())
    if "log-search-input" not in controls:
        raise ImeCheckError("log-search-input did not report bounds")
    target = "log-search-input"
    session.click_bounds(session.refresh(win), controls[target])
    subprocess.run(["fcitx5-remote", "-c"], env=session.env, capture_output=True)
    subprocess.run(
        ["fcitx5-remote", "-s", "keyboard-us"], env=session.env, capture_output=True
    )
    time.sleep(0.2)
    session.focus(win["wid"])
    session.x("xdotool", "type", "--clearmodifiers", "--delay", "40", "zz")
    time.sleep(0.3)
    ascii_copy = _copy_input(session, win)
    ascii_ok = ascii_copy.get("text") == "zz"
    result["phases"].append(
        {"name": "ascii-focus", "ok": ascii_ok, "copy": ascii_copy}
    )
    if not ascii_ok:
        failures.append("ASCII focus check did not read zz from the search field")
        return _finish(result, failures)
    session.key(win["wid"], "ctrl+a", "BackSpace")
    time.sleep(0.2)
    _clip_set(session, SENTINEL)

    def compose(label: str) -> dict:
        _activate_pinyin(session)
        bounds = parse_bounds(session.texts()).get(target)
        if bounds is None:
            raise ImeCheckError(f"{target} bounds disappeared before {label}")
        session.click_bounds(session.refresh(win), bounds)
        time.sleep(0.35)
        err_at = len(session.stderr_lines)
        session.key(win["wid"], "n", "i", "h", "a", "o", delay=120)
        time.sleep(0.3)

        def take():
            current = session.refresh(win)
            bounds = parse_bounds(session.texts()).get(target)
            shot = session.screenshot(current, label)
            return current, bounds, shot, _popup_at(shot["root"])

        current, bounds, shot, popup = _settle(take, lambda v: v[3] is not None)
        field = abs_bounds(current, bounds)
        association = (
            associated_candidate(popup, field, _window_rect(current))
            if popup and field
            else None
        )
        if field is not None:
            _crop_field(shot["window"], bounds, RUN / f"{label}-input.png")
        return {
            "field": field,
            "popup": popup,
            "association": association,
            "traces": session.traces(err_at),
            "screenshot": shot,
            "window": {k: current[k] for k in ("x", "y", "width", "height")},
        }

    initial = compose("02-preedit")
    result["preeditInitial"] = {k: initial[k] for k in initial if k != "screenshot"}
    result["screenshots"].append({"name": "02-preedit", **initial["screenshot"]})
    if not initial["popup"]:
        failures.append("no candidate popup in the root screenshot during nihao")
    elif not (initial["association"] or {}).get("anchored"):
        failures.append(
            "candidate popup is not anchored to log-search-input "
            f"({initial['association']})"
        )
    result["phases"].append(
        {
            "name": "preedit-position",
            "ok": bool(initial["popup"])
            and bool((initial["association"] or {}).get("anchored")),
            "association": initial["association"],
        }
    )

    session.key(win["wid"], "Escape")
    time.sleep(0.2)
    esc, esc_popup = _settle(
        lambda: _shoot(session, win, "03-escape"),
        lambda v: v[1] is None,
    )
    esc_copy = _copy_input(session, win)
    logs = session.texts()
    escape_ok = esc_popup is None and esc_copy.get("kind") == "sentinel_unchanged" and not any(
        "LOG_SEARCH" in line for line in logs[-30:]
    )
    result["escape"] = {
        "popup": esc_popup,
        "copy": esc_copy,
        "ok": escape_ok,
    }
    result["screenshots"].append({"name": "03-escape", **esc})
    result["phases"].append({"name": "escape", "ok": escape_ok})
    if not escape_ok:
        failures.append("Escape did not clear the candidate without searching")

    again = compose("04-preedit-before-resize")
    result["screenshots"].append(
        {"name": "04-preedit-before-resize", **again["screenshot"]}
    )
    before_popup = again["popup"]
    session.x("xdotool", "windowsize", win["wid"], "900", "600")
    try:
        session.wait_line(lambda line: "[APP:VIEWPORT: 900x600]" in line, timeout=8)
    except ImeCheckError as exc:
        failures.append(str(exc))
    time.sleep(0.3)

    def take_resize():
        resized = session.refresh(win)
        bounds = parse_bounds(session.texts()).get(target)
        field = abs_bounds(resized, bounds)
        shot = session.screenshot(resized, "05-resize")
        popup = _popup_at(shot["root"])
        association = (
            associated_candidate(popup, field, _window_rect(resized))
            if popup and field
            else None
        )
        moved = (
            before_popup is not None
            and popup is not None
            and abs(popup["y"] - before_popup["y"]) >= 40
        )
        return resized, field, shot, popup, association, moved

    resized, field, shot, popup, association, moved = _settle(
        take_resize, lambda v: bool(v[4] and v[4]["anchored"] and v[5])
    )
    resize_ok = bool(association and association["anchored"] and moved)
    result["resizeWhileComposing"] = {
        "window": {k: resized[k] for k in ("x", "y", "width", "height")},
        "field": field,
        "popupBefore": before_popup,
        "popup": popup,
        "association": association,
        "moved": moved,
    }
    result["screenshots"].append({"name": "05-resize", **shot})
    result["phases"].append({"name": "resize-while-composing", "ok": resize_ok})
    if not resize_ok:
        failures.append("candidate did not follow the field across the 900x600 resize")

    session.key(resized["wid"], "Escape")
    time.sleep(0.3)
    fresh = compose("06-preedit-after-resize")
    result["preeditAfterResize"] = {
        k: fresh[k] for k in fresh if k != "screenshot"
    }
    result["screenshots"].append(
        {"name": "06-preedit-after-resize", **fresh["screenshot"]}
    )
    fresh_ok = bool(fresh["popup"]) and bool(
        (fresh["association"] or {}).get("anchored")
    )
    result["phases"].append({"name": "preedit-after-resize", "ok": fresh_ok})
    if not fresh_ok:
        failures.append("fresh composition after resize is not caret-anchored")

    session.key(resized["wid"], "space")
    time.sleep(0.5)
    commit = _copy_input(session, resized)
    observed = commit.get("text") if commit.get("kind") == "text" else None
    if not (observed and any(ord(ch) > 127 for ch in observed)):
        session.key(resized["wid"], "1")
        time.sleep(0.4)
        commit = _copy_input(session, resized)
        observed = commit.get("text") if commit.get("kind") == "text" else None
    cjk = bool(observed) and any(ord(ch) > 127 for ch in observed)
    result["commit"] = {
        "copy": commit,
        "text": observed,
        "codepoints": codepoints(observed),
    }
    shot = session.screenshot(session.refresh(resized), "07-commit")
    result["screenshots"].append({"name": "07-commit", **shot})
    if not cjk:
        failures.append("Space did not commit a non-ASCII candidate into the field")
        result["phases"].append({"name": "commit", "ok": False})
        return _finish(result, failures)
    result["phases"].append({"name": "commit", "ok": True, "text": observed})

    before = len(session.lines)
    session.key(resized["wid"], "Return")
    try:
        session.wait_line(lambda line: "[APP:LOG_SEARCH:" in line, start=before, timeout=8)
        session.wait_line(lambda line: "[APP:GRAPH_LOADED:" in line, start=before, timeout=8)
    except ImeCheckError as exc:
        failures.append(str(exc))
    oracle = _git_grep(session.repo, observed or "")
    known = _git_grep(session.repo, KNOWN_PHRASE)
    git_shorts = sorted(row["short"] for row in oracle)
    # Full history: unmoved rows are not re-logged; CTRL_GONE drops stale ones.
    # GRAPH_LOADED precedes the repaint, so wait for the painted rows to match.
    rows = _settle(
        lambda: sorted(
            key.split(":", 1)[1]
            for key in parse_bounds(session.texts())
            if key.startswith("commit-row:")
        ),
        lambda v: v == git_shorts,
    )
    ui = sorted(set(rows))
    match = ui == git_shorts and KNOWN_PHRASE in (observed or "")
    result["search"] = {
        "observed": observed,
        "ui": ui,
        "git": oracle,
        "knownPhraseGit": known,
        "match": match,
    }
    shot = session.screenshot(session.refresh(resized), "08-search")
    result["screenshots"].append({"name": "08-search", **shot})
    result["phases"].append({"name": "search-vs-git", "ok": match, "ui": ui, "git": git_shorts})
    if not match:
        failures.append(f"UI rows {ui} differ from git grep {git_shorts} for {observed!r}")

    session.key(resized["wid"], "End", "shift+Left")
    time.sleep(0.15)
    one = _copy_input(session, resized, select_all=False)
    session.key(resized["wid"], "End", "BackSpace")
    time.sleep(0.25)
    rest = _copy_input(session, resized)
    expected_one = observed[-1] if observed else None
    expected_rest = observed[:-1] if observed else None
    unicode_ok = one.get("text") == expected_one and rest.get("text") == expected_rest
    result["unicodeEdit"] = {
        "shiftLeft": one,
        "afterBackspace": rest,
        "expectedOne": expected_one,
        "expectedRest": expected_rest,
        "ok": unicode_ok,
    }
    shot = session.screenshot(session.refresh(resized), "09-backspace")
    result["screenshots"].append({"name": "09-backspace", **shot})
    result["phases"].append({"name": "unicode-cursor-backspace", "ok": unicode_ok})
    if not unicode_ok:
        failures.append(
            f"unicode edit got {one.get('text')!r}/{rest.get('text')!r}, "
            f"expected {expected_one!r}/{expected_rest!r}"
        )

    session.key(resized["wid"], "ctrl+a", "BackSpace")
    time.sleep(0.2)
    _clip_set(session, SENTINEL)
    focus = compose("10-focus-preedit")
    result["screenshots"].append({"name": "10-focus-preedit", **focus["screenshot"]})
    head = parse_bounds(session.texts()).get("btn-head")
    if head is None:
        failures.append("btn-head bounds missing; focus switch not clicked")
        focus_limit = True
    else:
        session.click_bounds(session.refresh(resized), head)
        time.sleep(0.2)
        away, away_popup = _settle(
            lambda: _shoot(session, resized, "11-focus-away"),
            lambda v: v[1] is None,
        )
        result["screenshots"].append({"name": "11-focus-away", **away})
        back_bounds = parse_bounds(session.texts()).get(target)
        if back_bounds is not None:
            session.click_bounds(session.refresh(resized), back_bounds)
            time.sleep(0.3)
        back_copy = _copy_input(session, resized)
        focus_limit = away_popup is not None
        result["focusSwitch"] = {
            "popupRemained": away_popup is not None,
            "popup": away_popup,
            "fieldCopy": back_copy,
        }
    if focus_limit:
        failures.append(
            "Clicking btn-head keeps the Fcitx candidate; the vendored GPUI "
            "click-reset patch (vendor/gpui/SNIP_PATCH.md) is not in effect."
        )
    result["phases"].append(
        {"name": "focus-switch", "candidateCleared": not focus_limit}
    )
    return _finish(result, failures)


def _finish(result: dict, failures: list[str]) -> int:
    result["failures"] = failures
    if failures:
        return 2
    if result["platformLimits"]:
        return 3
    return 0


def _window_rect(win: dict) -> dict[str, int]:
    return rect(int(win["x"]), int(win["y"]), int(win["width"]), int(win["height"]))


def _viewport(lines: list[str]) -> str | None:
    found = None
    for line in lines:
        if "[APP:VIEWPORT:" in line:
            found = line
    return found


def _largest_popup(image) -> dict[str, int] | None:
    found = bright_popups(image)
    return found[0] if found else None


def _shoot(session: Session, win: dict, label: str) -> tuple[dict, dict | None]:
    shot = session.screenshot(session.refresh(win), label)
    return shot, _popup_at(shot["root"])


def _popup_at(path: str) -> dict[str, int] | None:
    from PIL import Image

    with Image.open(path) as image:
        return _largest_popup(image)


def _crop_field(window_png: str, bounds: tuple[int, int, int, int], dest: Path) -> None:
    x, y, w, h = bounds
    subprocess.run(
        [
            "convert",
            window_png,
            "-crop",
            f"{w + 8}x{h + 8}+{max(0, x - 4)}+{max(0, y - 4)}",
            "+repage",
            str(dest),
        ],
        check=False,
        capture_output=True,
        timeout=20,
    )


def _activate_pinyin(session: Session) -> None:
    subprocess.run(["fcitx5-remote", "-s", "pinyin"], env=session.env, capture_output=True)
    subprocess.run(["fcitx5-remote", "-o"], env=session.env, capture_output=True)
    time.sleep(0.2)
    name = subprocess.run(
        ["fcitx5-remote", "-n"], env=session.env, capture_output=True, text=True
    )
    if name.stdout.strip() != "pinyin":
        raise ImeCheckError(f"fcitx current IM is {name.stdout.strip()!r}, not pinyin")


def _clip_set(session: Session, data: bytes) -> None:
    # `xclip -i` forks and returns before the selection is owned; read it back.
    subprocess.run(
        ["xclip", "-selection", "clipboard", "-i"],
        input=data,
        env=session.env,
        check=True,
        timeout=5,
    )
    if not _wait(lambda: _clip_get(session)[0] == data, 3):
        raise ImeCheckError("clipboard did not hold the written data within 3s")


def _clip_get(session: Session) -> tuple[bytes | None, str | None]:
    res = subprocess.run(
        ["xclip", "-selection", "clipboard", "-o", "-t", "UTF8_STRING"],
        env=session.env,
        capture_output=True,
        timeout=5,
    )
    if res.returncode != 0:
        return None, res.stderr.decode("utf-8", "replace")[:200]
    return res.stdout, None


def _copy_input(session: Session, win: dict, select_all: bool = True) -> dict:
    before = len(session.lines)
    _clip_set(session, SENTINEL)
    if select_all:
        session.key(win["wid"], "ctrl+a")
    session.key(win["wid"], "ctrl+c")
    # Escape expects the sentinel to survive, so that phase spends the deadline.
    data, err = _settle(lambda: _clip_get(session), lambda v: v[0] != SENTINEL, 3)
    logs = [
        line
        for line in session.texts(before)
        if "COPY_" in line or "PREVIEW_COPIED" in line
    ]
    if data is None:
        return {"ok": False, "kind": "unread", "error": err, "copyLogs": logs}
    if data == SENTINEL:
        return {
            "ok": True,
            "kind": "sentinel_unchanged",
            "text": None,
            "codepoints": None,
            "copyLogs": logs,
        }
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as exc:
        return {"ok": True, "kind": "invalid_utf8", "error": str(exc), "copyLogs": logs}
    kind = "text"
    if text.startswith("// clipcode") or "clipcode-root" in text[:200]:
        kind = "clipcode_or_global_copy"
    return {
        "ok": kind == "text",
        "kind": kind,
        "text": text,
        "codepoints": codepoints(text),
        "copyLogs": logs,
    }


def _git_grep(repo: Path, query: str) -> list[dict]:
    res = subprocess.run(
        [
            "git",
            "-C",
            str(repo),
            "log",
            "--topo-order",
            "--format=%H%x00%s",
            "--branches",
            "--remotes",
            "--tags",
            "HEAD",
            "--fixed-strings",
            "--regexp-ignore-case",
            f"--grep={query}",
            "--",
        ],
        env=git_env(),
        capture_output=True,
        text=True,
    )
    rows = []
    for line in res.stdout.splitlines():
        if "\0" not in line:
            continue
        sha, subject = line.split("\0", 1)
        rows.append({"sha": sha, "short": sha[:7], "subject": subject})
    return rows


if __name__ == "__main__":
    sys.exit(main())
