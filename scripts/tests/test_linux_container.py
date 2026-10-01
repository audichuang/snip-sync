"""The Linux preflight container (scripts/linux-container) stays what CI's Linux jobs are.

`just preflight` on macOS runs CI's Linux jobs in this image, so a package or tool
version CI gains and the image lacks turns a local green into a CI red.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CI = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
DOCKERFILE = (ROOT / "scripts/linux-container/Dockerfile").read_text(encoding="utf-8")

# CI installs these; the image deliberately does not.
OMITTED = {
    "libegl1": "GPUI renders through Vulkan (lavapipe); nothing loads EGL",
    "libegl-mesa0": "GPUI renders through Vulkan (lavapipe); nothing loads EGL",
    "libgl1-mesa-dri": "xvfb depends on it, so apt installs it anyway",
    # The package job's runtime-only libraries; the -dev packages above pull them in.
    "libxcb1": "a dependency of libxcb1-dev",
    "libfontconfig1": "a dependency of libfontconfig1-dev",
    "libfreetype6": "a dependency of libfreetype6-dev",
}
# The image installs these beyond CI's apt lists: what a hosted runner has preinstalled.
PREINSTALLED_ON_RUNNERS = {
    "ca-certificates", "curl", "git", "pkg-config", "xz-utils", "python3", "shellcheck", "dbus",
    # ssh-keygen: core's signed-commit test (commits.rs) needs it.
    "openssh-client",
}


def apt_packages(text: str) -> set[str]:
    packages: set[str] = set()
    for match in re.finditer(r"apt-get install\b(?P<rest>(?:[^\n]*\\\n)*[^\n]*)", text):
        for token in match.group("rest").replace("\\\n", " ").split():
            if token == "&&":
                break
            if not token.startswith("-"):
                packages.add(token)
    return packages


class LinuxContainerMatchesCI(unittest.TestCase):
    def test_every_ci_package_is_installed_or_omitted_on_purpose(self) -> None:
        ci, image = apt_packages(CI), apt_packages(DOCKERFILE)
        self.assertTrue(ci and image, "no apt-get install found; the parser no longer matches")
        missing = sorted(ci - image - OMITTED.keys())
        self.assertEqual(missing, [], "CI installs these; add them to scripts/linux-container/Dockerfile")

    def test_omissions_and_extras_are_still_true(self) -> None:
        ci, image = apt_packages(CI), apt_packages(DOCKERFILE)
        self.assertEqual(sorted(OMITTED.keys() - ci), [], "CI no longer installs these; drop them from OMITTED")
        self.assertEqual(sorted(OMITTED.keys() & image), [], "the image installs these; drop them from OMITTED")
        extras = sorted(image - ci - PREINSTALLED_ON_RUNNERS)
        self.assertEqual(extras, [], "the image installs more than CI; add them to CI or drop them")

    def test_tool_versions_match_ci(self) -> None:
        ci_actionlint = re.search(r"ACTIONLINT_VERSION:\s*([\d.]+)", CI)
        image_actionlint = re.search(r"ARG ACTIONLINT_VERSION=([\d.]+)", DOCKERFILE)
        self.assertIsNotNone(ci_actionlint)
        self.assertIsNotNone(image_actionlint)
        self.assertEqual(image_actionlint.group(1), ci_actionlint.group(1))
        ci_node = set(re.findall(r"node-version:\s*(\d+)", CI))
        image_node = re.search(r"ARG NODE_MAJOR=(\d+)", DOCKERFILE)
        self.assertIsNotNone(image_node)
        self.assertEqual(ci_node, {image_node.group(1)})


if __name__ == "__main__":
    unittest.main()
