#!/usr/bin/env python3
"""
Unit tests for scripts/verify_artifacts.py:
Tests binary header parsing (ELF, Mach-O, PE), package bundle validation
(macOS .app, Linux tarball, Windows zip), checksum generation & verification,
path traversal security, and packaging with spaces in output path.
"""

from __future__ import annotations

import contextlib
import io
import os
import plistlib
import shutil
import struct
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

from scripts.verify_artifacts import (
    HDIUTIL_RETRY_DELAYS,
    HDIUTIL_TRANSIENT_ERRORS,
    VerificationError,
    audit_artifact_directory,
    compute_sha256,
    detect_binary_format_and_arch,
    detect_binary_format_and_arch_from_bytes,
    generate_checksums,
    main,
    verify_checksums_file,
    verify_dmg,
    verify_executable_binary,
    verify_macos_bundle,
    verify_tar_archive,
    verify_zip_archive,
)

CPU_X86_64 = 0x01000007
CPU_ARM64 = 0x0100000C
MAC_BINARY = "snip-sync.app/Contents/MacOS/snip-desktop-native"
MAC_PLIST = "snip-sync.app/Contents/Info.plist"
WIN_BINARY = "snip-sync/snip-desktop-native.exe"
WIN_README = "snip-sync/README.txt"


def make_elf(machine: int = 0x3E) -> bytes:
    """ELF64 executable with one PT_LOAD that fits the file."""
    file_size = 120
    buf = bytearray(file_size)
    buf[0:4] = b"\x7fELF"
    buf[4] = 2
    buf[5] = 1
    buf[6] = 1
    struct.pack_into("<HHI", buf, 16, 2, machine, 1)
    struct.pack_into("<QQQ", buf, 24, 0, 64, 0)
    struct.pack_into("<IHHHHHH", buf, 48, 0, 64, 56, 1, 0, 0, 0)
    struct.pack_into("<IIQQQQQQ", buf, 64, 1, 5, 0, 0x400000, 0, file_size, file_size, 8)
    return bytes(buf)


def make_macho(cputype: int, cpusubtype: int = 0) -> bytes:
    """Mach-O 64 MH_EXECUTE with one bounded load command."""
    header = struct.pack("<IIIIIIII", 0xFEEDFACF, cputype, cpusubtype, 2, 1, 24, 0, 0)
    return header + struct.pack("<II", 0x1B, 24) + (b"\x11" * 16)


def make_pe(machine: int = 0x8664) -> bytes:
    """PE32+ image with one section whose raw bytes fit the file."""
    e_lfanew = 0x80
    opt_size = 112
    sections_off = e_lfanew + 24 + opt_size
    raw_size = 16
    raw_ptr = sections_off + 40
    buf = bytearray(raw_ptr + raw_size)
    buf[0:2] = b"MZ"
    struct.pack_into("<I", buf, 0x3C, e_lfanew)
    buf[e_lfanew : e_lfanew + 4] = b"PE\x00\x00"
    struct.pack_into("<HHIIIHH", buf, e_lfanew + 4, machine, 1, 0, 0, 0, opt_size, 0x22)
    struct.pack_into("<H", buf, e_lfanew + 24, 0x10B if machine == 0x014C else 0x20B)
    buf[sections_off : sections_off + 5] = b".text"
    struct.pack_into("<IIII", buf, sections_off + 8, raw_size, 0x1000, raw_size, raw_ptr)
    return bytes(buf)


def make_fat(slices: list, *, fat64: bool = False, little: bool = False) -> bytes:
    """Fat Mach-O whose arch table points at the supplied thin images."""
    endian = "<" if little else ">"
    magic = 0xCAFEBABF if fat64 else 0xCAFEBABE
    arch_size = 32 if fat64 else 20
    align = 3
    align_bytes = 1 << align
    cursor = 8 + len(slices) * arch_size
    if cursor % align_bytes:
        cursor += align_bytes - (cursor % align_bytes)
    records = []
    for cputype, cpusubtype, thin in slices:
        records.append((cputype, cpusubtype, cursor, len(thin)))
        cursor += len(thin)
        if cursor % align_bytes:
            cursor += align_bytes - (cursor % align_bytes)
    buf = bytearray(cursor)
    struct.pack_into(endian + "I", buf, 0, magic)
    struct.pack_into(endian + "I", buf, 4, len(slices))
    for index, (cputype, cpusubtype, offset, size) in enumerate(records):
        base = 8 + index * arch_size
        if fat64:
            struct.pack_into(endian + "IIQQII", buf, base, cputype, cpusubtype, offset, size, align, 0)
        else:
            struct.pack_into(endian + "IIIII", buf, base, cputype, cpusubtype, offset, size, align)
        buf[offset : offset + size] = slices[index][2]
    return bytes(buf)


def app_plist(version: str = "0.1.4") -> bytes:
    return plistlib.dumps(
        {
            "CFBundleExecutable": "snip-desktop-native",
            "CFBundleIdentifier": "com.audichuang.snip-desktop-native",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
        }
    )


def desktop_entry(exec_line: str = "snip-desktop-native %u", version: str = "1.0") -> bytes:
    return (
        "[Desktop Entry]\n"
        "Name=Snip Sync Native\n"
        f"Exec={exec_line}\n"
        "Type=Application\n"
        f"Version={version}\n"
    ).encode()


def add_tar_bytes(tf: tarfile.TarFile, name: str, data: bytes, mode: int) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    tf.addfile(info, io.BytesIO(data))


LICENSE_NAMES = (
    "Inter-OFL.txt",
    "JetBrainsMono-OFL.txt",
    "expui-icons-LICENSE.txt",
    "expui-icons-NOTICE.txt",
)
MAC_LICENSES = "snip-sync.app/Contents/Resources/licenses"
WIN_LICENSES = "snip-sync/licenses"


def add_tar_licenses(tf: tarfile.TarFile, lic_dir: str) -> None:
    for name in LICENSE_NAMES:
        add_tar_bytes(tf, f"{lic_dir}/{name}", b"license text\n", 0o644)


def add_zip_licenses(zf: zipfile.ZipFile, lic_dir: str = WIN_LICENSES) -> None:
    for name in LICENSE_NAMES:
        zf.writestr(f"{lic_dir}/{name}", "license text\n")


def write_bundle_licenses(contents: Path) -> None:
    lic = contents / "Resources" / "licenses"
    lic.mkdir(parents=True, exist_ok=True)
    for name in LICENSE_NAMES:
        (lic / name).write_text("license text\n")


class TestVerifyArtifacts(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.test_dir = Path(self.temp_dir.name)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_detect_elf_headers(self) -> None:
        """Tests parsing ELF headers across architectures and endianness."""
        # 1. ELF 64-bit little endian x86_64
        p1 = self.test_dir / "bin_x86_64"
        p1.write_bytes(make_elf(0x3E))

        info = detect_binary_format_and_arch(p1)
        self.assertEqual(info["format"], "elf")
        self.assertEqual(info["arch"], "x86_64")
        self.assertEqual(info["bits"], 64)
        self.assertTrue(info["is_64bit"])

        # 2. ELF 64-bit little endian aarch64
        p2 = self.test_dir / "bin_arm64"
        p2.write_bytes(make_elf(0xB7))

        info2 = detect_binary_format_and_arch(p2)
        self.assertEqual(info2["format"], "elf")
        self.assertEqual(info2["arch"], "aarch64")

        # 3. Truncated file fails
        p_trunc = self.test_dir / "bin_trunc"
        p_trunc.write_bytes(b"\x7fELF")
        with self.assertRaises(VerificationError):
            detect_binary_format_and_arch(p_trunc)

    def test_detect_macho_headers(self) -> None:
        """Tests parsing Mach-O headers for x86_64 and arm64."""
        # 1. Mach-O 64-bit arm64
        p1 = self.test_dir / "macho_arm64"
        p1.write_bytes(make_macho(CPU_ARM64))

        info1 = detect_binary_format_and_arch(p1)
        self.assertEqual(info1["format"], "macho")
        self.assertEqual(info1["arch"], "aarch64")
        self.assertTrue(info1["is_64bit"])

        # 2. Mach-O 64-bit x86_64
        p2 = self.test_dir / "macho_x86_64"
        p2.write_bytes(make_macho(CPU_X86_64))

        info2 = detect_binary_format_and_arch(p2)
        self.assertEqual(info2["format"], "macho")
        self.assertEqual(info2["arch"], "x86_64")

    def test_detect_pe_headers(self) -> None:
        """Tests parsing PE / COFF headers for Windows x86_64 and arm64."""
        p1 = self.test_dir / "app.exe"
        p1.write_bytes(make_pe())

        info = detect_binary_format_and_arch(p1)
        self.assertEqual(info["format"], "pe")
        self.assertEqual(info["arch"], "x86_64")
        self.assertTrue(info["is_64bit"])

    def test_verify_macos_bundle(self) -> None:
        """Tests verifying macOS .app bundle structure, plist, and executable."""
        bundle = self.test_dir / "snip-sync.app"
        contents = bundle / "Contents"
        macos = contents / "MacOS"
        macos.mkdir(parents=True)
        write_bundle_licenses(macos.parent)

        bin_path = macos / "snip-desktop-native"
        bin_path.write_bytes(make_macho(CPU_ARM64))
        bin_path.chmod(0o755)
        (contents / "Info.plist").write_bytes(app_plist("0.1.4"))

        res = verify_macos_bundle(bundle, expected_version="0.1.4", expected_arch="aarch64")
        self.assertEqual(res["arch"], "aarch64")
        self.assertEqual(res["version"], "0.1.4")

        with self.assertRaises(VerificationError):
            verify_macos_bundle(bundle, expected_version="0.2.0")

        with self.assertRaises(VerificationError):
            verify_macos_bundle(bundle, expected_arch="x86_64")

    def test_verify_mac_tar_archive_with_plist(self) -> None:
        """
        Tests verifying a macOS .tar.gz archive with actual package layout:
        snip-sync.app/Contents/MacOS/snip-desktop-native
        snip-sync.app/Contents/Info.plist
        Verifies that it reads Info.plist stream to verify version.
        """
        tar_path = self.test_dir / "snip-sync_mac_arm.app.tar.gz"

        macho_arm64 = make_macho(CPU_ARM64)
        plist_bytes = app_plist("0.1.4")

        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, MAC_BINARY, macho_arm64, 0o755)
            add_tar_licenses(tf, MAC_LICENSES)
            add_tar_bytes(tf, MAC_PLIST, plist_bytes, 0o644)

        # Verification with target triple passes
        res = verify_tar_archive(tar_path, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertEqual(res["format"], "macho")
        self.assertEqual(res["arch"], "aarch64")

        # Version mismatch in plist raises
        with self.assertRaises(VerificationError):
            verify_tar_archive(tar_path, expected_version="0.2.0", target="aarch64-apple-darwin")

    def test_license_members_must_be_exact(self) -> None:
        """Every package carries exactly the third-party license set, non-empty, in its licenses/."""
        pkg = "snip-sync-0.1.4"

        def linux_tar(name: str, licenses: dict) -> Path:
            path = self.test_dir / name
            with tarfile.open(path, "w:gz") as tf:
                add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", make_elf(), 0o755)
                add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
                add_tar_bytes(tf, f"{pkg}/share/applications/snip-sync.desktop", desktop_entry(), 0o644)
                for member, data in licenses.items():
                    add_tar_bytes(tf, member, data, 0o644)
            return path

        full = {f"{pkg}/licenses/{n}": b"text\n" for n in LICENSE_NAMES}
        verify_tar_archive(linux_tar("ok.tar.gz", full), target="x86_64-unknown-linux-gnu", expected_version="0.1.4")

        missing = dict(full)
        missing.pop(f"{pkg}/licenses/expui-icons-NOTICE.txt")
        extra = {**full, f"{pkg}/licenses/Other-OFL.txt": b"text\n"}
        empty = {**full, f"{pkg}/licenses/Inter-OFL.txt": b""}
        moved = {f"{pkg}/share/licenses/{n}": b"text\n" for n in LICENSE_NAMES}
        for label, licenses, needle in (
            ("missing", missing, "must be exactly"),
            ("extra", extra, "must be exactly"),
            ("empty", empty, "Empty license"),
            ("moved", moved, "must be exactly"),
        ):
            with self.subTest(label):
                with self.assertRaises(VerificationError) as cm:
                    verify_tar_archive(
                        linux_tar(f"{label}.tar.gz", licenses),
                        target="x86_64-unknown-linux-gnu",
                        expected_version="0.1.4",
                    )
                self.assertIn(needle, str(cm.exception))

        zip_path = self.test_dir / "no-lic.zip"
        with zipfile.ZipFile(zip_path, "w") as zf:
            zf.writestr(WIN_BINARY, make_pe())
            zf.writestr(WIN_README, "Version: 0.1.4\n")
        with self.assertRaises(VerificationError) as cm:
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc", expected_version="0.1.4")
        self.assertIn(WIN_LICENSES, str(cm.exception))

        contents = self.test_dir / "snip-sync.app" / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "MacOS" / "snip-desktop-native").write_bytes(make_macho(CPU_ARM64))
        (contents / "MacOS" / "snip-desktop-native").chmod(0o755)
        (contents / "Info.plist").write_bytes(app_plist("0.1.4"))
        with self.assertRaises(VerificationError) as cm:
            verify_macos_bundle(contents.parent, expected_version="0.1.4")
        self.assertIn(MAC_LICENSES, str(cm.exception))

    def test_verify_tar_archive_version_and_security(self) -> None:
        """Tests verifying a Linux .tar.gz archive with exact version check and path traversal rejection."""
        elf_x86_64 = make_elf()
        pkg = "snip-sync-0.1.4"
        tar_path = self.test_dir / "snip-sync-linux-x86_64.tar.gz"
        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf_x86_64, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(
                tf,
                f"{pkg}/README.txt",
                b"snip-desktop-native\nVersion: 0.1.4\nTarget: x86_64-unknown-linux-gnu\n",
                0o644,
            )
            add_tar_bytes(
                tf,
                f"{pkg}/share/applications/snip-sync.desktop",
                desktop_entry(),
                0o644,
            )

        res = verify_tar_archive(
            tar_path,
            target="x86_64-unknown-linux-gnu",
            expected_version="0.1.4",
        )
        self.assertEqual(res["format"], "elf")
        self.assertEqual(res["arch"], "x86_64")

        # Version mismatch raises
        with self.assertRaises(VerificationError):
            verify_tar_archive(tar_path, expected_version="0.2.0", target="x86_64-unknown-linux-gnu")

        # Substring version like '10.1.40' raises (exact line match required)
        tar_bad_ver = self.test_dir / "tar_bad_ver.tar.gz"
        with tarfile.open(tar_bad_ver, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf_x86_64, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 10.1.40\n", 0o644)
            add_tar_bytes(
                tf,
                f"{pkg}/share/applications/snip-sync.desktop",
                desktop_entry(),
                0o644,
            )
        with self.assertRaises(VerificationError) as bad_ver:
            verify_tar_archive(tar_bad_ver, expected_version="0.1.4", target="x86_64-unknown-linux-gnu")
        self.assertIn("Version:", str(bad_ver.exception))

        # Malicious path traversal in tarball raises
        bad_tar = self.test_dir / "bad.tar.gz"
        with tarfile.open(bad_tar, "w:gz") as tf:
            ti = tarfile.TarInfo(name="../../etc/passwd")
            ti.size = 5
            tf.addfile(ti, io.BytesIO(b"evil\n"))
        with self.assertRaises(VerificationError):
            verify_tar_archive(bad_tar)

    def test_binary_format_mismatch_fails(self) -> None:
        """
        Tests that an ELF binary renamed to .exe and packaged in Windows zip
        fails format check when targeting x86_64-pc-windows-msvc.
        """
        zip_path = self.test_dir / "fake_windows.zip"
        elf_x86_64 = make_elf()

        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr("snip-sync/snip-desktop-native.exe", elf_x86_64)
                add_zip_licenses(zf)
                zf.writestr("snip-sync/README.txt", "Version: 0.1.4\n")

        with self.assertRaises(VerificationError) as cm:
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc", expected_version="0.1.4")
        self.assertIn("Binary format mismatch", str(cm.exception))

    def test_ambiguous_multiple_binaries_fail(self) -> None:
        """Tests that multiple binaries matching the expected name in an archive are rejected."""
        tar_path = self.test_dir / "ambiguous.tar.gz"
        elf_x86_64 = bytearray(64)
        elf_x86_64[:4] = b"\x7fELF"
        elf_x86_64[4] = 2
        elf_x86_64[5] = 1
        struct.pack_into("<H", elf_x86_64, 18, 0x3E)

        with tarfile.open(tar_path, "w:gz") as tf:
            ti1 = tarfile.TarInfo(name="bin/snip-desktop-native")
            ti1.size = len(elf_x86_64)
            ti1.mode = 0o755
            tf.addfile(ti1, io.BytesIO(elf_x86_64))

            ti2 = tarfile.TarInfo(name="backup/snip-desktop-native")
            ti2.size = len(elf_x86_64)
            ti2.mode = 0o755
            tf.addfile(ti2, io.BytesIO(elf_x86_64))

        with self.assertRaises(VerificationError) as cm:
            verify_tar_archive(tar_path, target="x86_64-unknown-linux-gnu")
        self.assertIn("Multiple/ambiguous", str(cm.exception))

    def test_verify_zip_archive_version_and_security(self) -> None:
        """Tests verifying a Windows .zip archive with version check and path traversal rejection."""
        zip_path = self.test_dir / "snip-sync-windows-x64.zip"
        pe_x86_64 = make_pe()

        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr(WIN_BINARY, pe_x86_64)
                add_zip_licenses(zf)
                zf.writestr(WIN_README, "Version: 0.1.4\nTarget: x86_64-pc-windows-msvc\n")

        res = verify_zip_archive(
            zip_path,
            target="x86_64-pc-windows-msvc",
            expected_version="0.1.4",
        )
        self.assertEqual(res["format"], "pe")
        self.assertEqual(res["arch"], "x86_64")

        with self.assertRaises(VerificationError):
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc", expected_version="0.9.9")

        # Dangerous path traversal with backslash in zip raises
        bad_zip = self.test_dir / "bad.zip"
        with open(bad_zip, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr("..\\evil.txt", b"evil")
        with self.assertRaises(VerificationError):
            verify_zip_archive(bad_zip)

    def test_checksum_complete_coverage(self) -> None:
        """Tests generating SHA256SUMS and asserting complete coverage of directory."""
        f1 = self.test_dir / "package-a.tar.gz"
        f1.write_bytes(b"package content a")
        f2 = self.test_dir / "package-b.zip"
        f2.write_bytes(b"package content b")

        sums_file = generate_checksums(self.test_dir, target_triple="x86_64-unknown-linux-gnu")
        self.assertTrue(sums_file.is_file())
        self.assertEqual(sums_file.name, "SHA256SUMS-x86_64-unknown-linux-gnu.txt")

        # Verification passes with complete coverage
        results = verify_checksums_file(sums_file, require_complete_coverage=True)
        self.assertEqual(len(results), 2)
        self.assertTrue(all(r[1] for r in results))

        # Adding an unlisted file causes complete-coverage failure
        untracked = self.test_dir / "sneaky-untracked.dmg"
        untracked.write_bytes(b"unlisted file")
        results2 = verify_checksums_file(sums_file, require_complete_coverage=True)
        failures = [r for r in results2 if not r[1]]
        self.assertEqual(len(failures), 1)
        self.assertEqual(failures[0][0], "sneaky-untracked.dmg")

        # Empty checksum file is strictly rejected
        empty_sums = self.test_dir / "empty_sums.txt"
        empty_sums.write_text("# Only comments\n\n", encoding="utf-8")
        with self.assertRaises(VerificationError):
            verify_checksums_file(empty_sums)

    def test_package_native_script_paths_with_spaces(self) -> None:
        """
        Tests package_native.sh execution when output path contains spaces,
        verifying that cd/absolute path handling works reliably.
        """
        bin_file = self.test_dir / "dummy_bin"
        bin_file.write_bytes(make_elf())
        bin_file.chmod(0o755)

        out_with_spaces = self.test_dir / "out dir with spaces"
        cmd = [
            "./scripts/package_native.sh",
            "x86_64-unknown-linux-gnu",
            str(out_with_spaces),
            str(bin_file),
            "0.1.4",
        ]
        res = subprocess.run(cmd, capture_output=True, text=True, check=False)
        self.assertEqual(res.returncode, 0, f"package_native.sh failed on path with spaces: {res.stderr}")

        # Verify produced tarball exists and passes artifact audit
        tarball = out_with_spaces / "snip-sync-linux-x86_64.tar.gz"
        self.assertTrue(tarball.is_file())

        audit_res = audit_artifact_directory(
            out_with_spaces,
            expected_version="0.1.4",
            target="x86_64-unknown-linux-gnu",
            require_checksums=True,
        )
        self.assertEqual(audit_res["artifacts_checked"], 1)
        self.assertTrue(audit_res["full_audit"])

    def test_unsupported_target_triple_rejected(self) -> None:
        """Tests that passing an unsupported target triple raises VerificationError."""
        with self.assertRaises(VerificationError) as cm:
            audit_artifact_directory(self.test_dir, target="riscv64-unknown-linux-gnu")
        self.assertIn("Unsupported target triple", str(cm.exception))

    def test_dmg_verification_non_darwin_status(self) -> None:
        """A garbage DMG is not a passed audit off Darwin, and not a size-only pass on Darwin."""
        dmg = self.test_dir / "test.dmg"
        dmg.write_bytes(b"A" * 2048)
        with self.assertRaises(VerificationError) as cm:
            verify_dmg(dmg)
        if os.uname().sysname != "Darwin":
            self.assertIn("non-Darwin", str(cm.exception))

    def test_macos_metadata_rejected_in_archives(self) -> None:
        """AppleDouble ._ files and __MACOSX, as a Mac's tar or zip adds them, fail the audit."""
        for extra in (
            "snip-sync.app/._Contents",
            "snip-sync.app/Contents/.__CodeSignature",
            "snip-sync.app/Contents/Resources/licenses/._Inter-OFL.txt",
        ):
            with self.subTest(extra=extra):
                tar_path = self.test_dir / "snip-sync_mac_arm.app.tar.gz"
                with tarfile.open(tar_path, "w:gz") as tf:
                    add_tar_bytes(tf, MAC_BINARY, make_macho(CPU_ARM64), 0o755)
                    add_tar_licenses(tf, MAC_LICENSES)
                    add_tar_bytes(tf, MAC_PLIST, app_plist("0.1.4"), 0o644)
                    add_tar_bytes(tf, extra, b"\0\5\26\7", 0o644)
                with self.assertRaises(VerificationError) as cm:
                    verify_tar_archive(tar_path, expected_version="0.1.4", target="aarch64-apple-darwin")
                self.assertIn("macOS metadata", str(cm.exception))

        zip_path = self.test_dir / "snip-sync-windows-x64.zip"
        with zipfile.ZipFile(zip_path, "w") as zf:
            zf.writestr(WIN_BINARY, make_pe())
            add_zip_licenses(zf)
            zf.writestr(WIN_README, "Version: 0.1.4\nTarget: x86_64-pc-windows-msvc\n")
            zf.writestr("__MACOSX/snip-sync/._README.txt", b"\0\5\26\7")
        with self.assertRaises(VerificationError) as cm:
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc", expected_version="0.1.4")
        self.assertIn("macOS metadata", str(cm.exception))

    def test_symlinks_rejected_in_tar_archive(self) -> None:
        """Tests that candidate tarball containing symlinks is rejected outright."""
        tar_path = self.test_dir / "has_symlink.tar.gz"
        elf_x86_64 = bytearray(64)
        elf_x86_64[:4] = b"\x7fELF"
        elf_x86_64[4] = 2
        elf_x86_64[5] = 1
        struct.pack_into("<H", elf_x86_64, 18, 0x3E)

        with tarfile.open(tar_path, "w:gz") as tf:
            ti_bin = tarfile.TarInfo(name="bin/snip-desktop-native")
            ti_bin.size = len(elf_x86_64)
            ti_bin.mode = 0o755
            tf.addfile(ti_bin, io.BytesIO(elf_x86_64))

            ti_sym = tarfile.TarInfo(name="bin/symlink")
            ti_sym.type = tarfile.SYMTYPE
            ti_sym.linkname = "snip-desktop-native"
            tf.addfile(ti_sym)

        with self.assertRaises(VerificationError) as cm:
            verify_tar_archive(tar_path, target="x86_64-unknown-linux-gnu")
        self.assertIn("Symlinks not permitted in candidate tarball", str(cm.exception))

    def test_symlinks_rejected_in_zip_archive(self) -> None:
        """Tests that candidate zip containing symlinks is rejected outright."""
        zip_path = self.test_dir / "has_symlink.zip"
        pe_x86_64 = bytearray(256)
        pe_x86_64[:2] = b"MZ"
        struct.pack_into("<I", pe_x86_64, 0x3C, 0x80)
        pe_x86_64[0x80:0x84] = b"PE\x00\x00"
        struct.pack_into("<H", pe_x86_64, 0x84, 0x8664)

        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr("snip-sync/snip-desktop-native.exe", pe_x86_64)
                add_zip_licenses(zf)
                zf.writestr("snip-sync/README.txt", "Version: 0.1.4\n")
                zinfo = zipfile.ZipInfo("snip-sync/link.exe")
                zinfo.create_system = 3  # Unix
                zinfo.external_attr = 0o120777 << 16  # S_IFLNK
                zf.writestr(zinfo, "snip-desktop-native.exe")

        with self.assertRaises(VerificationError) as cm:
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc")
        self.assertIn("Symlinks not permitted in candidate zip", str(cm.exception))

    def test_ambiguous_metadata_rejected_in_tar_archive(self) -> None:
        """Tests that multiple/ambiguous README.txt or Info.plist in candidate tarball are rejected."""
        # 1. Multiple README.txt
        tar_readme = self.test_dir / "mult_readme.tar.gz"
        elf_x86_64 = bytearray(64)
        elf_x86_64[:4] = b"\x7fELF"
        elf_x86_64[4] = 2
        elf_x86_64[5] = 1
        struct.pack_into("<H", elf_x86_64, 18, 0x3E)

        with tarfile.open(tar_readme, "w:gz") as tf:
            ti_bin = tarfile.TarInfo(name="bin/snip-desktop-native")
            ti_bin.size = len(elf_x86_64)
            ti_bin.mode = 0o755
            tf.addfile(ti_bin, io.BytesIO(elf_x86_64))

            ti_r1 = tarfile.TarInfo(name="README.txt")
            ti_r1.size = 15
            tf.addfile(ti_r1, io.BytesIO(b"Version: 0.1.4\n"))

            ti_r2 = tarfile.TarInfo(name="docs/README.txt")
            ti_r2.size = 15
            tf.addfile(ti_r2, io.BytesIO(b"Version: 0.1.4\n"))

        with self.assertRaises(VerificationError) as cm:
            verify_tar_archive(tar_readme, target="x86_64-unknown-linux-gnu", expected_version="0.1.4")
        self.assertIn("Ambiguous multiple README.txt", str(cm.exception))

        # 2. Multiple Info.plist
        tar_plist = self.test_dir / "mult_plist.tar.gz"
        with tarfile.open(tar_plist, "w:gz") as tf:
            ti_bin = tarfile.TarInfo(name="Contents/MacOS/snip-desktop-native")
            ti_bin.size = len(elf_x86_64)
            ti_bin.mode = 0o755
            tf.addfile(ti_bin, io.BytesIO(elf_x86_64))

            pbytes = b"<plist/>\n"
            ti_p1 = tarfile.TarInfo(name="Contents/Info.plist")
            ti_p1.size = len(pbytes)
            tf.addfile(ti_p1, io.BytesIO(pbytes))

            ti_p2 = tarfile.TarInfo(name="nested/Contents/Info.plist")
            ti_p2.size = len(pbytes)
            tf.addfile(ti_p2, io.BytesIO(pbytes))

        with self.assertRaises(VerificationError) as cm:
            verify_tar_archive(tar_plist, expected_binary_name="snip-desktop-native", expected_version="0.1.4")
        self.assertIn("Ambiguous multiple Info.plist", str(cm.exception))

    def test_ambiguous_metadata_rejected_in_zip_archive(self) -> None:
        """Tests that multiple/ambiguous README.txt in candidate zip are rejected."""
        zip_path = self.test_dir / "mult_readme.zip"
        pe_x86_64 = bytearray(256)
        pe_x86_64[:2] = b"MZ"
        struct.pack_into("<I", pe_x86_64, 0x3C, 0x80)
        pe_x86_64[0x80:0x84] = b"PE\x00\x00"
        struct.pack_into("<H", pe_x86_64, 0x84, 0x8664)

        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr("snip-sync/snip-desktop-native.exe", pe_x86_64)
                add_zip_licenses(zf)
                zf.writestr("snip-sync/README.txt", "Version: 0.1.4\n")
                zf.writestr("snip-sync/docs/README.txt", "Version: 0.1.4\n")

        with self.assertRaises(VerificationError) as cm:
            verify_zip_archive(zip_path, target="x86_64-pc-windows-msvc", expected_version="0.1.4")
        self.assertIn("Ambiguous multiple README.txt", str(cm.exception))

    def test_rejects_macho_stubs_java_and_truncated_fat(self) -> None:
        """Magic-only headers and Java class files are not usable Mach-O artifacts."""
        thin_stub = bytearray(64)
        thin_stub[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", thin_stub, 4, CPU_X86_64)
        thin_path = self.test_dir / "thin-stub"
        thin_path.write_bytes(thin_stub)
        with self.assertRaises(VerificationError):
            detect_binary_format_and_arch(thin_path)

        elf_stub = bytearray(64)
        elf_stub[:4] = b"\x7fELF"
        elf_stub[4] = 2
        elf_stub[5] = 1
        struct.pack_into("<H", elf_stub, 18, 0x3E)
        elf_path = self.test_dir / "elf-stub"
        elf_path.write_bytes(elf_stub)
        with self.assertRaises(VerificationError):
            detect_binary_format_and_arch(elf_path)

        java = bytearray(64)
        java[:4] = b"\xca\xfe\xba\xbe"
        struct.pack_into(">HH", java, 4, 0, 52)
        java_path = self.test_dir / "Fake.class"
        java_path.write_bytes(java)
        with self.assertRaises(VerificationError) as java_err:
            detect_binary_format_and_arch(java_path)
        self.assertIn("nfat_arch", str(java_err.exception))

        empty_fat = bytearray(64)
        empty_fat[:4] = b"\xca\xfe\xba\xbe"
        empty_path = self.test_dir / "empty-fat"
        empty_path.write_bytes(empty_fat)
        with self.assertRaises(VerificationError) as empty_err:
            detect_binary_format_and_arch(empty_path)
        self.assertIn("nfat_arch", str(empty_err.exception))

        truncated = bytearray(16)
        truncated[:4] = b"\xca\xfe\xba\xbe"
        struct.pack_into(">I", truncated, 4, 1)
        trunc_path = self.test_dir / "trunc-fat"
        trunc_path.write_bytes(truncated)
        with self.assertRaises(VerificationError):
            detect_binary_format_and_arch(trunc_path)

        # nfat_arch=1, only an ARM64 descriptor, slice offset/size zero.
        descriptor = bytearray(64)
        descriptor[:4] = b"\xca\xfe\xba\xbe"
        struct.pack_into(">I", descriptor, 4, 1)
        struct.pack_into(">I", descriptor, 8, CPU_ARM64)
        descriptor_path = self.test_dir / "arm-descriptor"
        descriptor_path.write_bytes(descriptor)
        with self.assertRaises(VerificationError):
            detect_binary_format_and_arch(descriptor_path)

        past = bytearray(64)
        past[:4] = b"\xca\xfe\xba\xbe"
        struct.pack_into(">I", past, 4, 1)
        struct.pack_into(">IIIII", past, 8, CPU_ARM64, 0, 128, 32, 3)
        past_path = self.test_dir / "slice-past-eof"
        past_path.write_bytes(past)
        with self.assertRaises(VerificationError) as past_err:
            detect_binary_format_and_arch(past_path)
        self.assertIn("exceeds file", str(past_err.exception))

        mismatched = make_fat([(CPU_X86_64, 0, make_macho(CPU_ARM64))])
        with self.assertRaises(VerificationError) as cpu_err:
            detect_binary_format_and_arch_from_bytes(mismatched)
        self.assertIn("does not match", str(cpu_err.exception))

    def _mac_tar(self, name: str, binary: bytes, *, binary_path: str = MAC_BINARY, plist_path: str = MAC_PLIST) -> Path:
        tar_path = self.test_dir / name
        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, binary_path, binary, 0o755)
            add_tar_licenses(tf, MAC_LICENSES)
            add_tar_bytes(tf, plist_path, app_plist("0.1.4"), 0o644)
        return tar_path

    def test_fat_slices_match_expected_arch_only(self) -> None:
        """A fat binary passes only targets whose CPU slice is present and well-formed."""
        arm = make_macho(CPU_ARM64)
        intel = make_macho(CPU_X86_64)
        arm_only = self._mac_tar("arm-only.tar.gz", make_fat([(CPU_ARM64, 0, arm)]))
        res_arm = verify_tar_archive(arm_only, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertEqual(res_arm["format"], "macho-fat")
        self.assertEqual(res_arm["slices"], ["aarch64"])
        with self.assertRaises(VerificationError) as wrong:
            verify_tar_archive(arm_only, expected_version="0.1.4", target="x86_64-apple-darwin")
        self.assertIn("architecture", str(wrong.exception))

        both = self._mac_tar(
            "both.tar.gz",
            make_fat([(CPU_ARM64, 0, arm), (CPU_X86_64, 0, intel)]),
        )
        for target, arch in (
            ("aarch64-apple-darwin", "aarch64"),
            ("x86_64-apple-darwin", "x86_64"),
        ):
            res = verify_tar_archive(both, expected_version="0.1.4", target=target)
            self.assertIn(arch, res["slices"])
            self.assertEqual(res["audit_scope"], "structural")

        fat64 = self._mac_tar("fat64.tar.gz", make_fat([(CPU_ARM64, 0, arm)], fat64=True))
        res64 = verify_tar_archive(fat64, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertEqual(res64["slices"], ["aarch64"])
        with self.assertRaises(VerificationError):
            verify_tar_archive(fat64, expected_version="0.1.4", target="x86_64-apple-darwin")

        swapped = self._mac_tar(
            "cigam.tar.gz",
            make_fat([(CPU_X86_64, 0, intel)], little=True),
        )
        res_swap = verify_tar_archive(swapped, expected_version="0.1.4", target="x86_64-apple-darwin")
        self.assertEqual(res_swap["slices"], ["x86_64"])
        with self.assertRaises(VerificationError):
            verify_tar_archive(swapped, expected_version="0.1.4", target="aarch64-apple-darwin")

    def test_review_probe_decoy_metadata_rejected(self) -> None:
        """The reported CAFEBABE stub plus a disconnected plist must not verify."""
        stub = bytearray(64)
        stub[:4] = b"\xca\xfe\xba\xbe"
        tar_path = self._mac_tar(
            "probe.tar.gz",
            bytes(stub),
            binary_path="not-a-bundle/snip-desktop-native",
            plist_path="decoy/Contents/Info.plist",
        )
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            with self.assertRaises(VerificationError):
                verify_tar_archive(tar_path, expected_version="0.1.4", target=target)

        placed = self._mac_tar("placed-stub.tar.gz", bytes(stub))
        with self.assertRaises(VerificationError) as placed_err:
            verify_tar_archive(placed, expected_version="0.1.4", target="x86_64-apple-darwin")
        self.assertIn("nfat_arch", str(placed_err.exception))

    def test_disconnected_plist_readme_and_desktop_spec_version(self) -> None:
        """Version metadata has to live in the same package as the binary."""
        disconnected = self._mac_tar(
            "disconnected-plist.tar.gz",
            make_macho(CPU_ARM64),
            plist_path="notes/Contents/Info.plist",
        )
        with self.assertRaises(VerificationError) as plist_err:
            verify_tar_archive(disconnected, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertIn("Info.plist", str(plist_err.exception))

        pkg = "snip-sync-0.1.4"
        elf = make_elf()
        readme_tar = self.test_dir / "disconnected-readme.tar.gz"
        with tarfile.open(readme_tar, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, "docs/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(tf, f"{pkg}/share/applications/snip-sync.desktop", desktop_entry(), 0o644)
        with self.assertRaises(VerificationError) as readme_err:
            verify_tar_archive(readme_tar, expected_version="0.1.4", target="x86_64-unknown-linux-gnu")
        self.assertIn("README", str(readme_err.exception))

        good_tar = self.test_dir / "desktop-spec.tar.gz"
        with tarfile.open(good_tar, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(tf, f"{pkg}/share/applications/snip-sync.desktop", desktop_entry(), 0o644)
        res = verify_tar_archive(good_tar, expected_version="0.1.4", target="x86_64-unknown-linux-gnu")
        self.assertEqual(res["format"], "elf")

        bad_exec = self.test_dir / "bad-exec.tar.gz"
        with tarfile.open(bad_exec, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(
                tf,
                f"{pkg}/share/applications/snip-sync.desktop",
                desktop_entry(exec_line="false"),
                0o644,
            )
        with self.assertRaises(VerificationError) as exec_err:
            verify_tar_archive(bad_exec, expected_version="0.1.4", target="x86_64-unknown-linux-gnu")
        self.assertIn("Exec", str(exec_err.exception))

        app_version_in_desktop = self.test_dir / "desktop-app-version.tar.gz"
        with tarfile.open(app_version_in_desktop, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(
                tf,
                f"{pkg}/share/applications/snip-sync.desktop",
                desktop_entry(version="0.1.4"),
                0o644,
            )
        with self.assertRaises(VerificationError) as spec_err:
            verify_tar_archive(
                app_version_in_desktop,
                expected_version="0.1.4",
                target="x86_64-unknown-linux-gnu",
            )
        self.assertIn("Desktop Entry spec", str(spec_err.exception))

        zip_path = self.test_dir / "disconnected-win.zip"
        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr(WIN_BINARY, make_pe())
                add_zip_licenses(zf)
                zf.writestr("other/README.txt", "Version: 0.1.4\n")
        with self.assertRaises(VerificationError) as zip_err:
            verify_zip_archive(zip_path, expected_version="0.1.4", target="x86_64-pc-windows-msvc")
        self.assertIn("README", str(zip_err.exception))

    def test_wrong_permissions_rejected(self) -> None:
        tar_path = self.test_dir / "mode.tar.gz"
        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, MAC_BINARY, make_macho(CPU_ARM64), 0o644)
            add_tar_licenses(tf, MAC_LICENSES)
            add_tar_bytes(tf, MAC_PLIST, app_plist(), 0o644)
        with self.assertRaises(VerificationError) as plain:
            verify_tar_archive(tar_path, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertIn("lacks execute", str(plain.exception))

        setuid = self.test_dir / "setuid.tar.gz"
        with tarfile.open(setuid, "w:gz") as tf:
            add_tar_bytes(tf, MAC_BINARY, make_macho(CPU_ARM64), 0o4755)
            add_tar_licenses(tf, MAC_LICENSES)
            add_tar_bytes(tf, MAC_PLIST, app_plist(), 0o644)
        with self.assertRaises(VerificationError) as special:
            verify_tar_archive(setuid, expected_version="0.1.4", target="aarch64-apple-darwin")
        self.assertIn("special permission", str(special.exception))

    def test_fat_inspect_is_bounded(self) -> None:
        arm = make_macho(CPU_ARM64)
        fat = make_fat([(CPU_ARM64, 0, arm)])
        self.assertGreater(len(fat), 48)
        with patch("scripts.verify_artifacts.MAX_INSPECT_BYTES", 48):
            with self.assertRaises(VerificationError) as cm:
                detect_binary_format_and_arch_from_bytes(fat)
        self.assertIn("inspect window", str(cm.exception))

    def _write_sums(self, directory: Path, names: list) -> None:
        import hashlib

        lines = []
        for name in names:
            digest = hashlib.sha256((directory / name).read_bytes()).hexdigest()
            lines.append(f"{digest}  {name}\n")
        (directory / "SHA256SUMS-test.txt").write_text("".join(lines), encoding="utf-8")

    def test_apple_audit_fails_closed_off_darwin(self) -> None:
        """Checksummed garbage, a missing DMG, or a tar-only directory is not an Apple audit."""
        garbage = self.test_dir / "garbage"
        garbage.mkdir()
        (garbage / "fake.dmg").write_bytes(b"A" * 2048)
        self._write_sums(garbage, ["fake.dmg"])
        with self.assertRaises(VerificationError):
            audit_artifact_directory(
                garbage,
                expected_version="0.1.4",
                target="aarch64-apple-darwin",
                require_checksums=True,
            )

        tar_only = self.test_dir / "tar-only"
        tar_only.mkdir()
        tar_bytes = self._mac_tar("ignored-name.tar.gz", make_macho(CPU_ARM64)).read_bytes()
        (tar_only / "snip-sync_mac_arm.app.tar.gz").write_bytes(tar_bytes)
        self._write_sums(tar_only, ["snip-sync_mac_arm.app.tar.gz"])
        with self.assertRaises(VerificationError) as missing:
            audit_artifact_directory(
                tar_only,
                expected_version="0.1.4",
                target="aarch64-apple-darwin",
                require_checksums=True,
            )
        self.assertIn("missing", str(missing.exception))

        both = self.test_dir / "both"
        both.mkdir()
        (both / "snip-sync_mac_arm.app.tar.gz").write_bytes(tar_bytes)
        (both / "snip-sync_mac_arm.dmg").write_bytes(b"A" * 2048)
        self._write_sums(both, ["snip-sync_mac_arm.app.tar.gz", "snip-sync_mac_arm.dmg"])
        with self.assertRaises(VerificationError) as unverified:
            audit_artifact_directory(
                both,
                expected_version="0.1.4",
                target="aarch64-apple-darwin",
                require_checksums=True,
            )
        if os.uname().sysname != "Darwin":
            self.assertIn("non-Darwin", str(unverified.exception))

    def test_platform_candidate_set_must_be_complete(self) -> None:
        linux_dir = self.test_dir / "linux-extra"
        linux_dir.mkdir()
        pkg = "snip-sync-0.1.4"
        elf = make_elf()
        tar_path = linux_dir / "snip-sync-linux-x86_64.tar.gz"
        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(tf, f"{pkg}/share/applications/snip-sync.desktop", desktop_entry(), 0o644)
        (linux_dir / "extra.zip").write_bytes(b"PK extra")
        self._write_sums(linux_dir, ["snip-sync-linux-x86_64.tar.gz", "extra.zip"])
        with self.assertRaises(VerificationError) as extra:
            audit_artifact_directory(
                linux_dir,
                expected_version="0.1.4",
                target="x86_64-unknown-linux-gnu",
                require_checksums=True,
            )
        self.assertIn("unexpected_files", str(extra.exception))

        windows_ok = self.test_dir / "windows-ok"
        windows_ok.mkdir()
        zip_path = windows_ok / "snip-sync-windows-x64.zip"
        with open(zip_path, "wb") as fp:
            with zipfile.ZipFile(fp, "w") as zf:
                zf.writestr(WIN_BINARY, make_pe())
                add_zip_licenses(zf)
                zf.writestr(WIN_README, "Version: 0.1.4\n")
        setup_path = windows_ok / "snip-sync-windows-setup.exe"
        # The Inno Setup stub is a 32-bit x86 PE followed by its payload.
        setup_path.write_bytes(make_pe(0x014C) + b"\0" * (1024 * 1024))
        self._write_sums(windows_ok, [zip_path.name, setup_path.name])
        windows_res = audit_artifact_directory(
            windows_ok,
            expected_version="0.1.4",
            target="x86_64-pc-windows-msvc",
            require_checksums=True,
        )
        self.assertTrue(windows_res["full_audit"])
        self.assertEqual(windows_res["artifacts_checked"], 2)

        for bad, reason in ((make_pe(0x014C), "suspiciously small"), (b"MZ" + b"\0" * (1024 * 1024), "")):
            setup_path.write_bytes(bad)
            self._write_sums(windows_ok, [zip_path.name, setup_path.name])
            with self.assertRaises(VerificationError) as bad_setup:
                audit_artifact_directory(
                    windows_ok,
                    expected_version="0.1.4",
                    target="x86_64-pc-windows-msvc",
                    require_checksums=True,
                )
            self.assertIn(reason, str(bad_setup.exception))

        windows_dir = self.test_dir / "windows-missing"
        windows_dir.mkdir()
        (windows_dir / "SHA256SUMS-test.txt").write_text("aa  missing.zip\n", encoding="utf-8")
        with self.assertRaises(VerificationError) as missing_zip:
            audit_artifact_directory(
                windows_dir,
                expected_version="0.1.4",
                target="x86_64-pc-windows-msvc",
                require_checksums=True,
            )
        self.assertIn("missing", str(missing_zip.exception))

    def test_package_native_darwin_missing_tools(self) -> None:
        binary = self.test_dir / "payload"
        binary.write_bytes(b"not a real macho")
        out = self.test_dir / "darwin-out"
        empty = self.test_dir / "empty-path"
        empty.mkdir()
        # Shebang is /usr/bin/env bash. Keep bash, and nothing that provides the Darwin tools.
        (empty / "bash").symlink_to("/bin/bash")
        env = os.environ.copy()
        env["PATH"] = str(empty)
        res = subprocess.run(
            [
                "./scripts/package_native.sh",
                "aarch64-apple-darwin",
                str(out),
                str(binary),
                "0.1.4",
            ],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("codesign", res.stderr)
        self.assertFalse(any(out.glob("*.dmg")) if out.exists() else False)
        self.assertFalse(any(out.glob("*.tar.gz")) if out.exists() else False)

        tools = self.test_dir / "partial-tools"
        tools.mkdir()
        (tools / "bash").symlink_to("/bin/bash")
        codesign = tools / "codesign"
        codesign.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        codesign.chmod(0o755)
        env["PATH"] = str(tools)
        res_hdiutil = subprocess.run(
            [
                "./scripts/package_native.sh",
                "x86_64-apple-darwin",
                str(self.test_dir / "darwin-out-2"),
                str(binary),
                "0.1.4",
            ],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        self.assertNotEqual(res_hdiutil.returncode, 0)
        self.assertIn("hdiutil", res_hdiutil.stderr)

    def test_dmg_readonly_mount_and_detach_failure(self) -> None:
        """Command shape only: this does not claim a Darwin hdiutil run succeeded."""
        dmg = self.test_dir / "shape.dmg"
        dmg.write_bytes(b"A" * 2048)
        mounts = []

        def side_effect(cmd, **_kwargs):
            if cmd[0] == "hdiutil" and cmd[1] == "attach":
                self.assertIn("-readonly", cmd)
                self.assertIn("-nobrowse", cmd)
                return self._fake_dmg_attach(cmd, mounts)
            if cmd[0] == "codesign":
                return subprocess.CompletedProcess(cmd, 0, "", "Signature=adhoc")
            if cmd[0] == "hdiutil" and cmd[1] == "detach":
                return subprocess.CompletedProcess(cmd, 1, "", "detach failed")
            return subprocess.CompletedProcess(cmd, 0, "", "")

        try:
            with patch("scripts.verify_artifacts.platform.system", return_value="Darwin"):
                with patch("scripts.verify_artifacts.subprocess.run", side_effect=side_effect):
                    with self.assertRaises(VerificationError) as cm:
                        verify_dmg(dmg, expected_version="0.1.4", expected_arch="aarch64")
            self.assertIn("detach", str(cm.exception))
        finally:
            for mnt in mounts:
                shutil.rmtree(mnt, ignore_errors=True)

    def _fake_dmg_attach(self, cmd: list, mounts: list) -> subprocess.CompletedProcess:
        """Stands in for a successful hdiutil attach: lays out a valid DMG volume at -mountpoint."""
        mnt = Path(cmd[cmd.index("-mountpoint") + 1])
        mounts.append(mnt)
        bundle = mnt / "snip-sync.app" / "Contents"
        macos = bundle / "MacOS"
        macos.mkdir(parents=True)
        write_bundle_licenses(macos.parent)
        binary = macos / "snip-desktop-native"
        binary.write_bytes(make_macho(CPU_ARM64))
        binary.chmod(0o755)
        (bundle / "Info.plist").write_bytes(app_plist())
        (mnt / "Applications").symlink_to("/Applications")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    def _run_dmg_with_flaky_attach(
        self, failures: int, error: str = "hdiutil: attach failed - Resource temporarily unavailable"
    ) -> tuple:
        """verify_dmg where the first `failures` attaches fail with `error`; sleeps are recorded, not taken."""
        dmg = self.test_dir / "flaky.dmg"
        dmg.write_bytes(b"A" * 2048)
        mounts: list = []
        calls: list = []
        sleeps: list = []

        def side_effect(cmd, **_kwargs):
            calls.append(cmd[:2])
            if cmd[0] == "hdiutil" and cmd[1] == "attach":
                if sum(c == ["hdiutil", "attach"] for c in calls) <= failures:
                    return subprocess.CompletedProcess(cmd, 1, "", error)
                return self._fake_dmg_attach(cmd, mounts)
            if cmd[0] == "codesign":
                return subprocess.CompletedProcess(cmd, 0, "", "Signature=adhoc")
            return subprocess.CompletedProcess(cmd, 0, "", "")

        try:
            with patch("scripts.verify_artifacts.platform.system", return_value="Darwin"), patch(
                "scripts.verify_artifacts.subprocess.run", side_effect=side_effect
            ), patch("scripts.verify_artifacts.time.sleep", side_effect=sleeps.append):
                try:
                    result = verify_dmg(dmg, expected_version="0.1.4", expected_arch="aarch64")
                except VerificationError as e:
                    result = e
        finally:
            for mnt in mounts:
                shutil.rmtree(mnt, ignore_errors=True)
        return result, calls, sleeps

    def test_dmg_attach_retries_transient_failures(self) -> None:
        """The CI failure of 2026-10-01: attach refused a few times, then worked."""
        failures = len(HDIUTIL_RETRY_DELAYS)
        result, calls, sleeps = self._run_dmg_with_flaky_attach(failures)
        self.assertNotIsInstance(result, VerificationError, str(result))
        self.assertTrue(result["verified_on_darwin"])
        self.assertEqual(calls.count(["hdiutil", "attach"]), failures + 1)
        self.assertEqual(calls.count(["hdiutil", "detach"]), 1)
        self.assertEqual(sleeps, list(HDIUTIL_RETRY_DELAYS))
        # Patient enough for a busy runner, still bounded.
        self.assertGreaterEqual(sum(HDIUTIL_RETRY_DELAYS), 45)
        self.assertLessEqual(sum(HDIUTIL_RETRY_DELAYS), 120)

    def test_dmg_attach_gives_up_after_every_retry(self) -> None:
        result, calls, sleeps = self._run_dmg_with_flaky_attach(failures=len(HDIUTIL_RETRY_DELAYS) + 1)
        self.assertIsInstance(result, VerificationError)
        self.assertIn(f"after {len(HDIUTIL_RETRY_DELAYS) + 1} attempt(s)", str(result))
        self.assertIn("Resource temporarily unavailable", str(result))
        self.assertEqual(sleeps, list(HDIUTIL_RETRY_DELAYS))
        # Nothing was mounted, so nothing is detached.
        self.assertNotIn(["hdiutil", "detach"], calls)

    def test_dmg_attach_does_not_retry_a_corrupt_image(self) -> None:
        """Only busy-runner errors are retried; a broken DMG fails at once."""
        result, calls, sleeps = self._run_dmg_with_flaky_attach(
            failures=1, error="hdiutil: attach failed - image not recognized"
        )
        self.assertIsInstance(result, VerificationError)
        self.assertIn("image not recognized", str(result))
        self.assertEqual(calls.count(["hdiutil", "attach"]), 1)
        self.assertEqual(sleeps, [])

    def _package_darwin_with_flaky_create(self, failures: int, error: str = "Resource busy") -> tuple:
        """package_native.sh with stub codesign/hdiutil/sleep/tar ahead of the real PATH."""
        run_dir = Path(tempfile.mkdtemp(prefix="flaky-create-", dir=self.test_dir))
        binary = run_dir / "payload"
        binary.write_bytes(b"stub binary")
        out = run_dir / "out"
        tools = run_dir / "tools"
        tools.mkdir()
        state = run_dir / "state"
        state.mkdir()
        stubs = {
            "codesign": "exit 0\n",
            # Fails its first $FAILS calls; on success writes the DMG path (the last argument).
            "hdiutil": (
                'n=$(($(cat "$STATE/count" 2>/dev/null || echo 0) + 1)); echo "$n" > "$STATE/count"\n'
                'if [ "$n" -le "$FAILS" ]; then echo "hdiutil: create failed - $ERROR" >&2; exit 1; fi\n'
                'for last; do :; done; echo dmg > "$last"\n'
            ),
            "sleep": 'echo "$1" >> "$STATE/sleeps"\n',
            # Records what the real tar would see, then runs it.
            "tar": 'echo "${COPYFILE_DISABLE-unset}" >> "$STATE/tar-copyfile"\nexec "$REAL_TAR" "$@"\n',
        }
        for name, body in stubs.items():
            stub = tools / name
            stub.write_text("#!/bin/sh\n" + body, encoding="utf-8")
            stub.chmod(0o755)
        env = os.environ.copy()
        real_tar = shutil.which("tar")
        self.assertIsNotNone(real_tar, "tar is missing")
        env["REAL_TAR"] = str(real_tar)
        env.pop("COPYFILE_DISABLE", None)
        env["PATH"] = f"{tools}{os.pathsep}{env['PATH']}"
        env["STATE"] = str(state)
        env["FAILS"] = str(failures)
        env["ERROR"] = error
        # BASH_ENV can rewrite PATH inside the script and bypass the stubs.
        env.pop("BASH_ENV", None)
        res = subprocess.run(
            ["./scripts/package_native.sh", "aarch64-apple-darwin", str(out), str(binary), "0.4.0"],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        sleeps_file = state / "sleeps"
        sleeps = [int(x) for x in sleeps_file.read_text().split()] if sleeps_file.exists() else []
        return res, out / "snip-sync_mac_arm.dmg", sleeps, state

    def test_package_native_tars_without_appledouble(self) -> None:
        """A Mac's tar turns extended attributes into ._ entries unless COPYFILE_DISABLE is set."""
        res, _dmg, _sleeps, state = self._package_darwin_with_flaky_create(failures=0)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual((state / "tar-copyfile").read_text().split(), ["1"])

    def test_package_native_retries_transient_dmg_create(self) -> None:
        # The shell loop retries the same errors on the same schedule as verify_dmg.
        failures = len(HDIUTIL_RETRY_DELAYS)
        for error in HDIUTIL_TRANSIENT_ERRORS:
            with self.subTest(error=error):
                res, dmg, sleeps, _ = self._package_darwin_with_flaky_create(failures, error)
                self.assertEqual(res.returncode, 0, res.stderr)
                self.assertTrue(dmg.is_file())
                self.assertEqual(sleeps, list(HDIUTIL_RETRY_DELAYS))

    def test_package_native_does_not_retry_a_hard_dmg_create_failure(self) -> None:
        res, dmg, sleeps, _ = self._package_darwin_with_flaky_create(failures=1, error="No space left on device")
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("No space left on device", res.stderr)
        self.assertIn("hdiutil create failed for", res.stderr)
        self.assertEqual(sleeps, [])

    def test_package_native_gives_up_after_every_dmg_create_retry(self) -> None:
        res, dmg, sleeps, _ = self._package_darwin_with_flaky_create(failures=len(HDIUTIL_RETRY_DELAYS) + 1)
        self.assertNotEqual(res.returncode, 0)
        self.assertIn(f"hdiutil create failed {len(HDIUTIL_RETRY_DELAYS) + 1} times", res.stderr)
        self.assertFalse(dmg.exists())
        self.assertEqual(sleeps, list(HDIUTIL_RETRY_DELAYS))

    def _linux_candidate(self, directory: Path, elf: bytes) -> None:
        directory.mkdir()
        pkg = "snip-sync-0.1.4"
        tar_path = directory / "snip-sync-linux-x86_64.tar.gz"
        with tarfile.open(tar_path, "w:gz") as tf:
            add_tar_bytes(tf, f"{pkg}/bin/snip-desktop-native", elf, 0o755)
            add_tar_licenses(tf, f"{pkg}/licenses")
            add_tar_bytes(tf, f"{pkg}/README.txt", b"Version: 0.1.4\n", 0o644)
            add_tar_bytes(
                tf,
                f"{pkg}/share/applications/snip-sync.desktop",
                desktop_entry(),
                0o644,
            )
        self._write_sums(directory, [tar_path.name])

    def _arm_fat(self) -> Path:
        path = self.test_dir / "arm-only.fat"
        path.write_bytes(make_fat([(CPU_ARM64, 0, make_macho(CPU_ARM64))]))
        path.chmod(0o755)
        return path

    def _cli(self, argv: list) -> tuple:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = main(argv)
        return code, stdout.getvalue(), stderr.getvalue()

    def test_target_constraints_reject_contradictions(self) -> None:
        """An explicit target owns arch and format. A conflicting override cannot pass."""
        x86_dir = self.test_dir / "linux-x86"
        self._linux_candidate(x86_dir, make_elf(0x3E))
        with self.assertRaises(VerificationError) as contradicted:
            audit_artifact_directory(
                x86_dir,
                expected_version="0.1.4",
                target="x86_64-unknown-linux-gnu",
                target_arch="aarch64",
                require_checksums=True,
            )
        self.assertIn("contradicts", str(contradicted.exception))

        matched = audit_artifact_directory(
            x86_dir,
            expected_version="0.1.4",
            target="x86_64-unknown-linux-gnu",
            target_arch="x86_64",
            require_checksums=True,
        )
        self.assertTrue(matched["full_audit"])
        self.assertEqual(matched["details"][0]["arch"], "x86_64")

        arm_dir = self.test_dir / "linux-arm-elf"
        self._linux_candidate(arm_dir, make_elf(0xB7))
        with self.assertRaises(VerificationError) as arm_override:
            audit_artifact_directory(
                arm_dir,
                expected_version="0.1.4",
                target="x86_64-unknown-linux-gnu",
                target_arch="aarch64",
                require_checksums=True,
            )
        self.assertIn("contradicts", str(arm_override.exception))
        with self.assertRaises(VerificationError) as arm_target:
            audit_artifact_directory(
                arm_dir,
                expected_version="0.1.4",
                target="x86_64-unknown-linux-gnu",
                require_checksums=True,
            )
        self.assertIn("architecture", str(arm_target.exception))

        mac_tar = self._mac_tar("alias-arm.tar.gz", make_macho(CPU_ARM64))
        alias = verify_tar_archive(
            mac_tar,
            expected_version="0.1.4",
            target="aarch64-apple-darwin",
            expected_arch="arm64",
        )
        self.assertEqual(alias["slices"], ["aarch64"])
        with self.assertRaises(VerificationError) as bad_format:
            verify_tar_archive(
                mac_tar,
                target="aarch64-apple-darwin",
                expected_format="elf",
            )
        self.assertIn("contradicts", str(bad_format.exception))

        dummy_zip = self.test_dir / "dummy.zip"
        dummy_zip.write_bytes(b"not-a-zip")
        with self.assertRaises(VerificationError) as bad_zip_arch:
            verify_zip_archive(
                dummy_zip,
                target="x86_64-pc-windows-msvc",
                expected_arch="aarch64",
            )
        self.assertIn("contradicts", str(bad_zip_arch.exception))

        fat = self._arm_fat()
        code, out, err = self._cli(["--file", str(fat), "--target", "x86_64-apple-darwin"])
        self.assertEqual(code, 1, out + err)
        self.assertIn("architecture", err)
        self.assertNotIn("Verified", out)

        code, out, err = self._cli(
            ["--file", str(fat), "--target", "aarch64-apple-darwin", "--arch", "arm64"]
        )
        self.assertEqual(code, 0, err)
        self.assertIn("aarch64", out)

        code, _out, err = self._cli(
            ["--file", str(fat), "--target", "aarch64-apple-darwin", "--arch", "x86_64"]
        )
        self.assertEqual(code, 1)
        self.assertIn("contradicts", err)

        code, _out, err = self._cli(["--file", str(fat), "--target", "riscv64-unknown-linux-gnu"])
        self.assertEqual(code, 1)
        self.assertIn("Unsupported target triple", err)

        # No target: structural recognition of the same fat still succeeds.
        code, out, _err = self._cli(["--file", str(fat)])
        self.assertEqual(code, 0, _err)
        self.assertIn("aarch64", out)

        bundle = self.test_dir / "snip-sync.app"
        macos = bundle / "Contents" / "MacOS"
        macos.mkdir(parents=True)
        write_bundle_licenses(macos.parent)
        binary = macos / "snip-desktop-native"
        binary.write_bytes(make_macho(CPU_X86_64))
        binary.chmod(0o755)
        (bundle / "Contents" / "Info.plist").write_bytes(app_plist())
        code, _out, err = self._cli(
            ["--file", str(bundle), "--target", "x86_64-unknown-linux-gnu"]
        )
        self.assertEqual(code, 1)
        self.assertIn("incompatible", err)


if __name__ == "__main__":
    unittest.main()
