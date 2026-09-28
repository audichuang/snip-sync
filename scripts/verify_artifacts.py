#!/usr/bin/env python3
"""
scripts/verify_artifacts.py

Strict verification tool for native installation packages and build artifacts.
Audits binary architectures (ELF, Mach-O thin/fat, PE), package bundle structure
(macOS .app, Linux tarball, Windows zip, macOS DMG), version strings,
checksum integrity (complete coverage without unlisted files), and macOS ad-hoc
code signatures without claiming notarization.

Mach-O fat binaries are accepted only when the arch table and the referenced
thin slices are inside the file and the expected CPU is actually present.
Apple publication audits require both the tar.gz and the dmg, and the dmg
must have been mounted and checked on Darwin. A structural tar read is not
that audit.

Uses Python 3 standard library only.
Stream-based archive reading avoids unsafe disk extractions.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import os
import platform
import plistlib
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path
from typing import Any, BinaryIO, Dict, List, Optional, Sequence, Tuple


class VerificationError(Exception):
    """Raised when an artifact fails verification."""


SUPPORTED_TARGETS: Dict[str, Dict[str, Any]] = {
    "x86_64-unknown-linux-gnu": {
        "os": "linux",
        "format": "elf",
        "arch": "x86_64",
        "binary_name": "snip-desktop-native",
    },
    "x86_64-pc-windows-msvc": {
        "os": "windows",
        "format": "pe",
        "arch": "x86_64",
        "binary_name": "snip-desktop-native.exe",
    },
    "aarch64-apple-darwin": {
        "os": "macos",
        "format": "macho",
        "arch": "aarch64",
        "binary_name": "snip-desktop-native",
    },
    "x86_64-apple-darwin": {
        "os": "macos",
        "format": "macho",
        "arch": "x86_64",
        "binary_name": "snip-desktop-native",
    },
}

# Documented package_native outputs. A targeted audit must see this set and nothing else.
CANDIDATE_FILES: Dict[str, Tuple[str, ...]] = {
    "aarch64-apple-darwin": (
        "snip-sync_mac_arm.app.tar.gz",
        "snip-sync_mac_arm.dmg",
    ),
    "x86_64-apple-darwin": (
        "snip-sync_mac_intel.app.tar.gz",
        "snip-sync_mac_intel.dmg",
    ),
    "x86_64-unknown-linux-gnu": ("snip-sync-linux-x86_64.tar.gz",),
    "x86_64-pc-windows-msvc": ("snip-sync-windows-x64.zip", "snip-sync-windows-setup.exe"),
}

MAC_BINARY = "snip-sync.app/Contents/MacOS/snip-desktop-native"
MAC_PLIST = "snip-sync.app/Contents/Info.plist"
WIN_BINARY = "snip-sync/snip-desktop-native.exe"
WIN_README = "snip-sync/README.txt"
LINUX_PKG_PREFIX = "snip-sync-"
DESKTOP_FILE_NAME = "snip-sync.desktop"
# Third-party license texts staged by package_native.sh (stage_licenses).
LICENSES_DIR_NAME = "licenses"
LICENSE_FILES = (
    "Inter-OFL.txt",
    "JetBrainsMono-OFL.txt",
    "expui-icons-LICENSE.txt",
    "expui-icons-NOTICE.txt",
)
MAC_LICENSES_DIR = "snip-sync.app/Contents/Resources/licenses"
WIN_LICENSES_DIR = "snip-sync/licenses"

# Bounds for archive inspection. Large enough for a real universal binary's
# slice headers, small enough that a hostile member cannot force an unbounded read.
MAX_BINARY_BYTES = 512 * 1024 * 1024
MAX_INSPECT_BYTES = 512 * 1024 * 1024
MAX_METADATA_BYTES = 64 * 1024
MAX_FAT_ARCH = 8
MAX_MACHO_CMDS = 4096
MAX_MACHO_CMDS_BYTES = 4 * 1024 * 1024
MAX_ELF_PHDRS = 128
MAX_PE_SECTIONS = 96
MAX_PE_OPTIONAL = 1024
MAX_PE_LFANEW = 1 * 1024 * 1024

FAT_MAGICS = {
    b"\xca\xfe\xba\xbe": (">", False),
    b"\xbe\xba\xfe\xca": ("<", False),
    b"\xca\xfe\xba\xbf": (">", True),
    b"\xbf\xba\xfe\xca": ("<", True),
}
THIN_MAGICS = {
    b"\xcf\xfa\xed\xfe": ("<", 64),
    b"\xfe\xed\xfa\xcf": (">", 64),
    b"\xce\xfa\xed\xfe": ("<", 32),
    b"\xfe\xed\xfa\xce": (">", 32),
}
CPU_TYPE_X86_64 = 0x01000007
CPU_TYPE_ARM64 = 0x0100000C
CPU_ARCH_NAMES = {
    CPU_TYPE_X86_64: "x86_64",
    CPU_TYPE_ARM64: "aarch64",
    7: "x86",
    12: "arm",
}
MH_EXECUTE = 0x2
ELF_MACHINES = {0x3E: "x86_64", 0xB7: "aarch64", 0x03: "x86"}
PE_MACHINES = {
    0x014C: ("x86", 32, 0x10B),
    0x8664: ("x86_64", 64, 0x20B),
    0xAA64: ("aarch64", 64, 0x20B),
}


def check_safe_archive_path(path_str: str) -> None:
    """
    Rejects absolute paths, Windows drive paths, and path traversal components.
    """
    if path_str.startswith("/") or path_str.startswith("\\"):
        raise VerificationError(f"Dangerous absolute path in archive: {path_str}")
    if len(path_str) >= 2 and path_str[1] == ":" and path_str[0].isalpha():
        raise VerificationError(f"Dangerous Windows drive path in archive: {path_str}")
    normalized = path_str.replace("\\", "/")
    parts = normalized.split("/")
    if ".." in parts:
        raise VerificationError(f"Dangerous path traversal in archive: {path_str}")


def _unknown_binary() -> Dict[str, Any]:
    return {
        "format": "unknown",
        "bits": 0,
        "endian": "unknown",
        "arch": "unknown",
        "is_64bit": False,
        "slices": [],
    }


def _range_fits(offset: int, length: int, size: int) -> bool:
    if offset < 0 or length < 0 or size < 0:
        return False
    if offset > size:
        return False
    return length <= size - offset


def _normalize_arch(arch: str) -> str:
    key = arch.strip().lower()
    if key in ("arm64", "aarch64"):
        return "aarch64"
    return key


def _formats_agree(explicit: str, target_format: str) -> bool:
    left = explicit.strip().lower()
    right = target_format.strip().lower()
    if left == right:
        return True
    macho = {"macho", "macho-fat"}
    return left in macho and right in macho


def _artifact_kind_matches(kind: str, os_name: str) -> bool:
    if kind == "binary":
        return True
    if kind in ("app", "dmg"):
        return os_name == "macos"
    if kind == "zip":
        return os_name == "windows"
    if kind == "tar":
        return os_name in ("linux", "macos")
    return False


def resolve_verification_constraints(
    target: Optional[str],
    *,
    arch: Optional[str] = None,
    binary_format: Optional[str] = None,
    binary_name: Optional[str] = None,
    artifact_kind: Optional[str] = None,
) -> Dict[str, Optional[str]]:
    """A supported target owns arch, format, binary name, and artifact kind.

    An explicit constraint that disagrees with that target is rejected.
    arm64 and aarch64 are the same architecture. With no target, the call
    stays a structural check and only normalizes an explicit architecture.
    """
    norm_arch = _normalize_arch(arch) if arch else None
    if not target:
        return {
            "target": None,
            "os": None,
            "format": binary_format,
            "arch": norm_arch,
            "binary_name": binary_name,
        }
    if target not in SUPPORTED_TARGETS:
        raise VerificationError(
            f"Unsupported target triple: '{target}'. Supported: {list(SUPPORTED_TARGETS.keys())}"
        )
    conf = SUPPORTED_TARGETS[target]
    if norm_arch and norm_arch != conf["arch"]:
        raise VerificationError(
            f"Architecture '{arch}' contradicts target {target} ({conf['arch']})"
        )
    if binary_format and not _formats_agree(binary_format, conf["format"]):
        raise VerificationError(
            f"Format '{binary_format}' contradicts target {target} ({conf['format']})"
        )
    if binary_name and binary_name != conf["binary_name"]:
        raise VerificationError(
            f"Binary name '{binary_name}' contradicts target {target} ({conf['binary_name']})"
        )
    if artifact_kind and not _artifact_kind_matches(artifact_kind, conf["os"]):
        raise VerificationError(
            f"Target {target} ({conf['os']}) is incompatible with {artifact_kind} artifacts"
        )
    return {
        "target": target,
        "os": conf["os"],
        "format": conf["format"],
        "arch": conf["arch"],
        "binary_name": conf["binary_name"],
    }


def _file_artifact_kind(path: Path) -> str:
    name = path.name
    if name.endswith(".tar.gz") or name.endswith(".tgz"):
        return "tar"
    if name.endswith(".dmg"):
        return "dmg"
    if name.endswith(".zip"):
        return "zip"
    if name.endswith(".app"):
        return "app"
    return "binary"


def _require_arch(bin_info: Dict[str, Any], expected_arch: Optional[str], where: str) -> None:
    """Match an expected CPU against parsed slices. A fat label is not a match."""
    if not expected_arch:
        return
    norm_expected = _normalize_arch(expected_arch)
    slices = [_normalize_arch(item) for item in bin_info.get("slices") or []]
    if slices:
        if norm_expected not in slices:
            raise VerificationError(
                f"Binary architecture mismatch in {where}: expected {norm_expected}, "
                f"slices {slices}"
            )
        return
    actual = _normalize_arch(str(bin_info.get("arch", "")))
    if actual != norm_expected:
        raise VerificationError(
            f"Binary architecture mismatch in {where}: expected {norm_expected}, got {actual}"
        )


def _require_format(bin_info: Dict[str, Any], expected_format: Optional[str], where: str) -> None:
    if not expected_format:
        return
    actual = str(bin_info.get("format", "unknown")).lower()
    expected = expected_format.lower()
    if expected == "macho" and actual in ("macho", "macho-fat"):
        return
    if actual != expected:
        raise VerificationError(
            f"Binary format mismatch in {where}: expected {expected_format}, got {bin_info.get('format')}"
        )


class _PrefixReader:
    """Sequential bounded reader. Refuses windows past the file or the inspect cap."""

    def __init__(self, stream: BinaryIO, file_size: int, cap: int) -> None:
        self.stream = stream
        self.file_size = file_size
        self.cap = cap
        self.buf = bytearray()

    def read_until(self, end: int) -> bytes:
        if end < 0 or end > self.file_size:
            raise VerificationError(
                f"inspect offset {end} outside binary size {self.file_size}"
            )
        if end > self.cap:
            raise VerificationError(
                f"inspect window {end} exceeds bound of {self.cap} bytes"
            )
        while len(self.buf) < end:
            chunk = self.stream.read(end - len(self.buf))
            if not chunk:
                raise VerificationError(
                    f"binary truncated: needed {end} bytes, got {len(self.buf)}"
                )
            self.buf.extend(chunk)
        return bytes(self.buf)

    def snapshot(self) -> bytes:
        return bytes(self.buf)


def _magic_kind(buf: bytes) -> str:
    if len(buf) >= 4 and buf[:4] == b"\x7fELF":
        return "elf"
    if len(buf) >= 4 and buf[:4] in FAT_MAGICS:
        return "fat"
    if len(buf) >= 4 and buf[:4] in THIN_MAGICS:
        return "thin"
    if len(buf) >= 2 and buf[:2] == b"MZ":
        return "pe"
    return "unknown"


def _cpu_name(cputype: int) -> str:
    return CPU_ARCH_NAMES.get(cputype, f"unknown-macho-0x{cputype:08x}")


def _macho_header_fields(buf: bytes, offset: int) -> Dict[str, Any]:
    if offset < 0 or offset + 4 > len(buf):
        raise VerificationError("Mach-O magic is outside the inspected prefix")
    raw = buf[offset : offset + 4]
    if raw not in THIN_MAGICS:
        raise VerificationError(f"thin Mach-O magic mismatch at offset {offset}")
    endian, bits = THIN_MAGICS[raw]
    header_size = 32 if bits == 64 else 28
    if offset + header_size > len(buf):
        raise VerificationError("Mach-O header exceeds inspected prefix")
    if bits == 64:
        cputype, cpusubtype, filetype, ncmds, sizeofcmds, _flags, _reserved = struct.unpack_from(
            endian + "IIIIIII", buf, offset + 4
        )
    else:
        cputype, cpusubtype, filetype, ncmds, sizeofcmds, _flags = struct.unpack_from(
            endian + "IIIIII", buf, offset + 4
        )
    return {
        "endian": endian,
        "bits": bits,
        "header_size": header_size,
        "cputype": cputype,
        "cpusubtype": cpusubtype,
        "filetype": filetype,
        "ncmds": ncmds,
        "sizeofcmds": sizeofcmds,
    }


def _validate_macho_commands(fields: Dict[str, Any], file_size: int, offset: int, slice_end: int) -> int:
    bits = fields["bits"]
    align = 8 if bits == 64 else 4
    ncmds = fields["ncmds"]
    sizeofcmds = fields["sizeofcmds"]
    if fields["filetype"] != MH_EXECUTE:
        raise VerificationError(
            f"Mach-O filetype 0x{fields['filetype']:x} is not MH_EXECUTE"
        )
    if ncmds < 1 or ncmds > MAX_MACHO_CMDS:
        raise VerificationError(f"Mach-O ncmds {ncmds} is outside 1..{MAX_MACHO_CMDS}")
    if (
        sizeofcmds < ncmds * align
        or sizeofcmds > MAX_MACHO_CMDS_BYTES
        or sizeofcmds % align != 0
    ):
        raise VerificationError(f"Mach-O sizeofcmds {sizeofcmds} is out of bounds")
    cmds_end = offset + fields["header_size"] + sizeofcmds
    if cmds_end > slice_end or cmds_end > file_size:
        raise VerificationError("Mach-O load commands exceed the slice or file")
    return cmds_end


def _parse_thin_macho(buf: bytes, file_size: int, offset: int, slice_end: int) -> Dict[str, Any]:
    fields = _macho_header_fields(buf, offset)
    cmds_end = _validate_macho_commands(fields, file_size, offset, slice_end)
    if cmds_end > len(buf):
        raise VerificationError("Mach-O load commands were not in the inspected prefix")
    endian = fields["endian"]
    align = 8 if fields["bits"] == 64 else 4
    pos = offset + fields["header_size"]
    for _ in range(fields["ncmds"]):
        if pos + 8 > cmds_end:
            raise VerificationError("truncated Mach-O load command")
        _cmd, cmdsize = struct.unpack_from(endian + "II", buf, pos)
        if cmdsize < align or cmdsize % align != 0 or pos + cmdsize > cmds_end:
            raise VerificationError(f"Mach-O cmdsize {cmdsize} is out of bounds")
        pos += cmdsize
    if pos != cmds_end:
        raise VerificationError("Mach-O load commands do not fill sizeofcmds")
    arch = _cpu_name(fields["cputype"])
    slices = [] if arch.startswith("unknown-") else [arch]
    return {
        "format": "macho",
        "bits": fields["bits"],
        "endian": "little" if endian == "<" else "big",
        "arch": arch,
        "is_64bit": fields["bits"] == 64,
        "slices": slices,
        "cputype": fields["cputype"],
        "cpusubtype": fields["cpusubtype"],
    }


def _ensure_thin_prefix(reader: _PrefixReader, file_size: int, offset: int, slice_end: int) -> None:
    header_need = offset + 32
    if not _range_fits(offset, 32, file_size):
        raise VerificationError("Mach-O header exceeds file")
    buf = reader.read_until(header_need)
    fields = _macho_header_fields(buf, offset)
    cmds_end = _validate_macho_commands(fields, file_size, offset, slice_end)
    reader.read_until(cmds_end)


def _parse_fat_table(buf: bytes, file_size: int) -> Tuple[str, bool, List[Tuple[int, int, int, int]]]:
    if len(buf) < 8 or file_size < 8:
        raise VerificationError("fat header truncated")
    magic = buf[:4]
    if magic not in FAT_MAGICS:
        raise VerificationError("not a fat Mach-O")
    endian, is64 = FAT_MAGICS[magic]
    nfat = struct.unpack_from(endian + "I", buf, 4)[0]
    if nfat < 1 or nfat > MAX_FAT_ARCH:
        raise VerificationError(f"nfat_arch {nfat} is outside 1..{MAX_FAT_ARCH}")
    arch_size = 32 if is64 else 20
    table_end = 8 + nfat * arch_size
    if table_end > file_size or table_end > len(buf):
        raise VerificationError("fat arch table exceeds file")
    slices: List[Tuple[int, int, int, int]] = []
    seen = set()
    for index in range(nfat):
        base = 8 + index * arch_size
        if is64:
            cputype, cpusubtype, offset, size, align, reserved = struct.unpack_from(
                endian + "IIQQII", buf, base
            )
            if reserved != 0:
                raise VerificationError("fat64 reserved field is not zero")
        else:
            cputype, cpusubtype, offset, size, align = struct.unpack_from(
                endian + "IIIII", buf, base
            )
        if align > 31:
            raise VerificationError(f"fat align {align} is out of bounds")
        if not _range_fits(offset, size, file_size):
            raise VerificationError("fat slice offset/size exceeds file")
        if offset < table_end:
            raise VerificationError("fat slice overlaps the arch table")
        if size < 32:
            raise VerificationError("fat slice is too small for a Mach-O header")
        if offset % (1 << align) != 0:
            raise VerificationError("fat slice offset is not aligned")
        if cputype in seen:
            raise VerificationError(f"duplicate fat cputype 0x{cputype:08x}")
        seen.add(cputype)
        slices.append((cputype, cpusubtype, offset, size))
    for i, (_, _, off_a, size_a) in enumerate(slices):
        end_a = off_a + size_a
        for _, _, off_b, size_b in slices[i + 1 :]:
            end_b = off_b + size_b
            if off_a < end_b and off_b < end_a:
                raise VerificationError("fat slices overlap")
    return endian, is64, slices


def _parse_fat(buf: bytes, file_size: int) -> Dict[str, Any]:
    endian, _is64, slices = _parse_fat_table(buf, file_size)
    parsed = []
    for cputype, cpusubtype, offset, size in slices:
        thin = _parse_thin_macho(buf, file_size, offset, offset + size)
        if thin["cputype"] != cputype or thin["cpusubtype"] != cpusubtype:
            raise VerificationError(
                "fat arch table CPU does not match the thin Mach-O header "
                f"(table 0x{cputype:08x}/0x{cpusubtype:08x}, "
                f"header 0x{thin['cputype']:08x}/0x{thin['cpusubtype']:08x})"
            )
        parsed.append(thin)
    archs = [item["arch"] for item in parsed]
    known = [item for item in archs if not str(item).startswith("unknown-")]
    return {
        "format": "macho-fat",
        "bits": 64 if parsed and all(item["bits"] == 64 for item in parsed) else 0,
        "endian": "little" if endian == "<" else "big",
        "arch": known[0] if len(known) == 1 else "multi",
        "is_64bit": bool(parsed) and all(item["is_64bit"] for item in parsed),
        "slices": known,
    }


def _inspect_fat(reader: _PrefixReader, file_size: int) -> Dict[str, Any]:
    if file_size < 8:
        raise VerificationError("fat header truncated")
    buf = reader.read_until(8)
    magic = buf[:4]
    endian, is64 = FAT_MAGICS[magic]
    nfat = struct.unpack_from(endian + "I", buf, 4)[0]
    if nfat < 1 or nfat > MAX_FAT_ARCH:
        raise VerificationError(f"nfat_arch {nfat} is outside 1..{MAX_FAT_ARCH}")
    table_end = 8 + nfat * (32 if is64 else 20)
    if table_end > file_size:
        raise VerificationError("fat arch table exceeds file")
    buf = reader.read_until(table_end)
    _endian, _is64, slices = _parse_fat_table(buf, file_size)
    for _cputype, _subtype, offset, size in sorted(slices, key=lambda item: item[2]):
        _ensure_thin_prefix(reader, file_size, offset, offset + size)
    return _parse_fat(reader.snapshot(), file_size)


def _parse_elf(buf: bytes, file_size: int) -> Dict[str, Any]:
    if len(buf) < 16 or file_size < 16 or buf[:4] != b"\x7fELF":
        raise VerificationError("ELF truncated before e_ident")
    ei_class = buf[4]
    ei_data = buf[5]
    ei_version = buf[6]
    if ei_class not in (1, 2):
        raise VerificationError(f"ELF EI_CLASS {ei_class} is not 32-bit or 64-bit")
    if ei_data not in (1, 2):
        raise VerificationError(f"ELF EI_DATA {ei_data} is not an endian flag")
    if ei_version != 1:
        raise VerificationError("ELF EI_VERSION is not EV_CURRENT")
    bits = 64 if ei_class == 2 else 32
    endian = "<" if ei_data == 1 else ">"
    ehsize = 64 if bits == 64 else 52
    if file_size < ehsize or len(buf) < ehsize:
        raise VerificationError(f"ELF header truncated ({file_size} bytes, need {ehsize})")
    if bits == 64:
        e_type, e_machine, e_version = struct.unpack_from(endian + "HHI", buf, 16)
        e_phoff, e_shoff = struct.unpack_from(endian + "QQ", buf, 32)
        e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx = struct.unpack_from(
            endian + "HHHHHH", buf, 52
        )
        ph_struct = endian + "IIQQQQQQ"
    else:
        e_type, e_machine, e_version = struct.unpack_from(endian + "HHI", buf, 16)
        _entry, e_phoff, e_shoff = struct.unpack_from(endian + "III", buf, 24)
        e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx = struct.unpack_from(
            endian + "HHHHHH", buf, 40
        )
        ph_struct = endian + "IIIIIIII"
    if e_version != 1:
        raise VerificationError("ELF e_version is not EV_CURRENT")
    if e_ehsize != ehsize:
        raise VerificationError(f"ELF e_ehsize {e_ehsize} does not match class {ei_class}")
    if e_type not in (2, 3):
        raise VerificationError(f"ELF e_type {e_type} is not ET_EXEC or ET_DYN")
    if e_phnum < 1 or e_phnum > MAX_ELF_PHDRS:
        raise VerificationError(f"ELF e_phnum {e_phnum} is outside 1..{MAX_ELF_PHDRS}")
    phentsize = 56 if bits == 64 else 32
    if e_phentsize != phentsize:
        raise VerificationError(f"ELF e_phentsize {e_phentsize} is not {phentsize}")
    ph_bytes = e_phnum * e_phentsize
    if not _range_fits(e_phoff, ph_bytes, file_size):
        raise VerificationError("ELF program headers exceed file")
    ph_end = e_phoff + ph_bytes
    if ph_end > len(buf):
        raise VerificationError("ELF program headers were not in the inspected prefix")
    if e_shnum == 0:
        if e_shoff != 0 or e_shstrndx not in (0, 0xFFFF):
            raise VerificationError("ELF section header fields are inconsistent with e_shnum 0")
    else:
        shentsize = 64 if bits == 64 else 40
        if e_shentsize != shentsize:
            raise VerificationError(f"ELF e_shentsize {e_shentsize} is not {shentsize}")
        if not _range_fits(e_shoff, e_shnum * e_shentsize, file_size):
            raise VerificationError("ELF section headers exceed file")
        if e_shstrndx != 0xFFFF and e_shstrndx >= e_shnum:
            raise VerificationError("ELF e_shstrndx is outside the section table")
    found_load = False
    for index in range(e_phnum):
        off = e_phoff + index * e_phentsize
        fields = struct.unpack_from(ph_struct, buf, off)
        if bits == 64:
            p_type, _flags, p_offset, _vaddr, _paddr, p_filesz, _memsz, p_align = fields
        else:
            p_type, p_offset, _vaddr, _paddr, p_filesz, _memsz, _flags, p_align = fields
        if p_type != 1:
            continue
        if p_filesz and not _range_fits(p_offset, p_filesz, file_size):
            raise VerificationError("ELF PT_LOAD exceeds file")
        if p_align > 1 and (p_align & (p_align - 1)) != 0:
            raise VerificationError("ELF PT_LOAD alignment is not a power of two")
        found_load = True
    if not found_load:
        raise VerificationError("ELF has no PT_LOAD segment")
    arch = ELF_MACHINES.get(e_machine, f"unknown-elf-0x{e_machine:04x}")
    slices = [] if arch.startswith("unknown-") else [arch]
    return {
        "format": "elf",
        "bits": bits,
        "endian": "little" if endian == "<" else "big",
        "arch": arch,
        "is_64bit": bits == 64,
        "slices": slices,
    }


def _inspect_elf(reader: _PrefixReader, file_size: int) -> Dict[str, Any]:
    if file_size < 16:
        raise VerificationError("ELF truncated before e_ident")
    buf = reader.read_until(16)
    ei_class = buf[4]
    ei_data = buf[5]
    if ei_class not in (1, 2) or ei_data not in (1, 2):
        raise VerificationError("ELF ident class or endian is invalid")
    ehsize = 64 if ei_class == 2 else 52
    if file_size < ehsize:
        raise VerificationError(f"ELF header truncated ({file_size} bytes, need {ehsize})")
    buf = reader.read_until(ehsize)
    endian = "<" if ei_data == 1 else ">"
    if ei_class == 2:
        e_phoff = struct.unpack_from(endian + "Q", buf, 32)[0]
        e_phentsize, e_phnum = struct.unpack_from(endian + "HH", buf, 54)
    else:
        e_phoff = struct.unpack_from(endian + "I", buf, 28)[0]
        e_phentsize, e_phnum = struct.unpack_from(endian + "HH", buf, 42)
    if e_phnum < 1 or e_phnum > MAX_ELF_PHDRS or e_phentsize > 1024:
        raise VerificationError("ELF program header count or size is out of bounds")
    ph_bytes = e_phnum * e_phentsize
    if not _range_fits(e_phoff, ph_bytes, file_size):
        raise VerificationError("ELF program headers exceed file")
    reader.read_until(e_phoff + ph_bytes)
    return _parse_elf(reader.snapshot(), file_size)


def _parse_pe(buf: bytes, file_size: int) -> Dict[str, Any]:
    if file_size < 0x40 or len(buf) < 0x40 or buf[:2] != b"MZ":
        raise VerificationError("PE truncated before e_lfanew")
    e_lfanew = struct.unpack_from("<I", buf, 0x3C)[0]
    if e_lfanew < 0x40 or e_lfanew > MAX_PE_LFANEW or not _range_fits(e_lfanew, 24, file_size):
        raise VerificationError(f"PE e_lfanew {e_lfanew} is out of bounds")
    if len(buf) < e_lfanew + 24:
        raise VerificationError("PE COFF header was not in the inspected prefix")
    if buf[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        raise VerificationError("PE signature missing")
    machine, nsections, _ts, _symptr, _nsym, opt_size, _chars = struct.unpack_from(
        "<HHIIIHH", buf, e_lfanew + 4
    )
    if nsections < 1 or nsections > MAX_PE_SECTIONS:
        raise VerificationError(f"PE section count {nsections} is outside 1..{MAX_PE_SECTIONS}")
    if opt_size < 2 or opt_size > MAX_PE_OPTIONAL:
        raise VerificationError(f"PE SizeOfOptionalHeader {opt_size} is out of bounds")
    opt_off = e_lfanew + 24
    sections_off = opt_off + opt_size
    sections_end = sections_off + nsections * 40
    if not _range_fits(opt_off, opt_size + nsections * 40, file_size) or sections_end > len(buf):
        raise VerificationError("PE optional header or section table exceeds file")
    magic = struct.unpack_from("<H", buf, opt_off)[0]
    if machine in PE_MACHINES:
        arch, bits, expect_magic = PE_MACHINES[machine]
        if magic != expect_magic:
            raise VerificationError(
                f"PE optional magic 0x{magic:04x} does not match machine 0x{machine:04x}"
            )
    else:
        if magic not in (0x10B, 0x20B):
            raise VerificationError(f"PE optional magic 0x{magic:04x} is not PE32 or PE32+")
        arch = f"unknown-pe-0x{machine:04x}"
        bits = 64 if magic == 0x20B else 32
    for index in range(nsections):
        off = sections_off + index * 40
        _vsize, _va, raw_size, raw_ptr = struct.unpack_from("<IIII", buf, off + 8)
        if raw_size and not _range_fits(raw_ptr, raw_size, file_size):
            raise VerificationError(f"PE section {index} raw data exceeds file")
    slices = [] if arch.startswith("unknown-") else [arch]
    return {
        "format": "pe",
        "bits": bits,
        "endian": "little",
        "arch": arch,
        "is_64bit": bits == 64,
        "slices": slices,
    }


def _inspect_pe(reader: _PrefixReader, file_size: int) -> Dict[str, Any]:
    if file_size < 0x40:
        raise VerificationError("PE truncated before e_lfanew")
    buf = reader.read_until(0x40)
    e_lfanew = struct.unpack_from("<I", buf, 0x3C)[0]
    if e_lfanew < 0x40 or e_lfanew > MAX_PE_LFANEW or not _range_fits(e_lfanew, 24, file_size):
        raise VerificationError(f"PE e_lfanew {e_lfanew} is out of bounds")
    buf = reader.read_until(e_lfanew + 24)
    if buf[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        raise VerificationError("PE signature missing")
    _machine, nsections, _ts, _symptr, _nsym, opt_size, _chars = struct.unpack_from(
        "<HHIIIHH", buf, e_lfanew + 4
    )
    if nsections < 1 or nsections > MAX_PE_SECTIONS or opt_size < 2 or opt_size > MAX_PE_OPTIONAL:
        raise VerificationError("PE section count or optional header size is out of bounds")
    sections_end = e_lfanew + 24 + opt_size + nsections * 40
    if sections_end > file_size:
        raise VerificationError("PE section table exceeds file")
    reader.read_until(sections_end)
    return _parse_pe(reader.snapshot(), file_size)


def inspect_binary_stream(stream: BinaryIO, file_size: int) -> Dict[str, Any]:
    """Parse a binary from a stream without extracting it or reading past the cap."""
    if file_size < 4:
        raise VerificationError(f"Binary file too small ({file_size} bytes)")
    if file_size > MAX_BINARY_BYTES:
        raise VerificationError(
            f"Binary size {file_size} exceeds inspect bound of {MAX_BINARY_BYTES} bytes"
        )
    reader = _PrefixReader(stream, file_size, MAX_INSPECT_BYTES)
    head = reader.read_until(min(file_size, 8))
    kind = _magic_kind(head)
    if kind == "unknown":
        if file_size < 64:
            raise VerificationError(
                f"Header buffer too short ({file_size} bytes) for binary analysis"
            )
        return _unknown_binary()
    if kind == "elf":
        return _inspect_elf(reader, file_size)
    if kind == "pe":
        return _inspect_pe(reader, file_size)
    if kind == "thin":
        _ensure_thin_prefix(reader, file_size, 0, file_size)
        return _parse_thin_macho(reader.snapshot(), file_size, 0, file_size)
    if kind == "fat":
        return _inspect_fat(reader, file_size)
    raise VerificationError(f"Unhandled binary kind {kind}")


def detect_binary_format_and_arch_from_bytes(
    header: bytes, file_size: Optional[int] = None
) -> Dict[str, Any]:
    """Parse ELF, Mach-O, or PE headers from an in-memory prefix of the file."""
    size = len(header) if file_size is None else file_size
    return inspect_binary_stream(io.BytesIO(header), size)


def detect_binary_format_and_arch(file_path: Path) -> Dict[str, Any]:
    """Reads a binary from disk and detects format, architecture, and slice CPUs."""
    if not file_path.is_file():
        raise VerificationError(f"Binary file not found: {file_path}")
    if file_path.is_symlink():
        raise VerificationError(f"Binary path is a symlink: {file_path}")
    size = file_path.stat().st_size
    with file_path.open("rb") as handle:
        return inspect_binary_stream(handle, size)


def compute_sha256(file_path: Path) -> str:
    """Computes SHA-256 hex digest for a file."""
    h = hashlib.sha256()
    with file_path.open("rb") as f:
        while chunk := f.read(65536):
            h.update(chunk)
    return h.hexdigest()


def generate_checksums(directory: Path, target_triple: Optional[str] = None) -> Path:
    """Generates target-specific or general SHA256SUMS file."""
    filename = f"SHA256SUMS-{target_triple}.txt" if target_triple else "SHA256SUMS.txt"
    sums_file = directory / filename
    entries = []
    for item in sorted(directory.glob("*")):
        if item.is_file() and not item.name.startswith("SHA256SUMS"):
            digest = compute_sha256(item)
            entries.append(f"{digest}  {item.name}\n")

    sums_file.write_text("".join(entries), encoding="utf-8")
    return sums_file


def verify_checksums_file(sums_path: Path, require_complete_coverage: bool = True) -> List[Tuple[str, bool, str]]:
    """
    Verifies checksums in file. Requires at least one entry, and optionally asserts
    complete coverage (every artifact file in the folder is accounted for).
    """
    if not sums_path.is_file():
        raise VerificationError(f"Checksum file not found: {sums_path}")

    base_dir = sums_path.parent
    lines = [ln.strip() for ln in sums_path.read_text(encoding="utf-8").splitlines() if ln.strip() and not ln.startswith("#")]

    if not lines:
        raise VerificationError(f"Checksum file {sums_path} contains 0 valid entries (empty file rejected)")

    results = []
    covered_files = set()

    for line in lines:
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        expected_sha, rel_name = parts
        rel_name = rel_name.lstrip("*")
        target = base_dir / rel_name
        covered_files.add(rel_name)

        if not target.is_file():
            results.append((rel_name, False, f"Missing file: {target}"))
            continue

        actual_sha = compute_sha256(target)
        if actual_sha.lower() != expected_sha.lower():
            results.append((rel_name, False, f"Checksum mismatch: expected {expected_sha}, got {actual_sha}"))
        else:
            results.append((rel_name, True, "OK"))

    # Assert complete coverage: no untracked artifact files in directory
    if require_complete_coverage:
        for p in base_dir.glob("*"):
            if p.is_file() and not p.name.startswith("SHA256SUMS"):
                if p.name not in covered_files:
                    results.append((p.name, False, "File present in directory but missing from checksums file"))

    return results


def _version_token(expected_version: str) -> str:
    return expected_version.lstrip("v")


def _accepted_linux_packages(expected_version: Optional[str]) -> Optional[set]:
    if not expected_version:
        return None
    clean = _version_token(expected_version)
    return {
        f"{LINUX_PKG_PREFIX}{clean}",
        f"{LINUX_PKG_PREFIX}{expected_version}",
    }


def _linux_package_name(binary: str, binary_name: str) -> Optional[str]:
    parts = binary.split("/")
    if len(parts) != 3 or parts[1] != "bin" or parts[2] != binary_name:
        return None
    package = parts[0]
    if not package.startswith(LINUX_PKG_PREFIX) or package == LINUX_PKG_PREFIX:
        return None
    return package


def _layout_kind(binary: str, binary_name: str) -> Optional[str]:
    if binary == MAC_BINARY and binary_name == "snip-desktop-native":
        return "macos"
    if binary == WIN_BINARY and binary_name == "snip-desktop-native.exe":
        return "windows"
    if _linux_package_name(binary, binary_name):
        return "linux"
    return None


def _load_plist(data: bytes) -> Dict[str, Any]:
    if not data:
        raise VerificationError("Info.plist is empty")
    try:
        plist = plistlib.loads(data)
    except Exception as exc:
        raise VerificationError(f"Failed parsing Info.plist: {exc}") from exc
    if not isinstance(plist, dict):
        raise VerificationError("Info.plist root is not a dict")
    return plist


def _validate_plist(plist: Dict[str, Any], expected_version: Optional[str]) -> str:
    if plist.get("CFBundlePackageType") != "APPL":
        raise VerificationError(
            f"Expected CFBundlePackageType 'APPL', got '{plist.get('CFBundlePackageType')}'"
        )
    if plist.get("CFBundleExecutable") != "snip-desktop-native":
        raise VerificationError(
            "CFBundleExecutable must be 'snip-desktop-native', "
            f"got '{plist.get('CFBundleExecutable')}'"
        )
    short_version = plist.get("CFBundleShortVersionString")
    if expected_version:
        clean = _version_token(expected_version)
        if short_version != clean:
            raise VerificationError(
                f"Version mismatch in Info.plist: expected '{clean}', got '{short_version}'"
            )
        if plist.get("CFBundleVersion") != clean:
            raise VerificationError(
                f"CFBundleVersion mismatch in Info.plist: expected '{clean}', "
                f"got '{plist.get('CFBundleVersion')}'"
            )
    return short_version if isinstance(short_version, str) else ""


def _validate_readme(text: str, expected_version: str, label: str) -> None:
    clean = _version_token(expected_version)
    lines = [line.strip() for line in text.splitlines()]
    if f"Version: {clean}" in lines or f"Version: v{clean}" in lines:
        return
    raise VerificationError(
        f"Exact 'Version: {clean}' line not found in {label}. Lines: {lines}"
    )


def _desktop_argv0(exec_value: str) -> str:
    value = exec_value.strip()
    if not value:
        return ""
    if value[0] in ("'", '"'):
        end = value.find(value[0], 1)
        if end < 0:
            return ""
        return value[1:end]
    return value.split()[0]


def _validate_desktop(text: str) -> None:
    """Check layout and Exec. Version=1.0 is the desktop-entry spec, not the app."""
    in_entry = False
    saw_entry = False
    fields: Dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            in_entry = line == "[Desktop Entry]"
            saw_entry = saw_entry or in_entry
            continue
        if in_entry and "=" in line:
            key, value = line.split("=", 1)
            fields[key.strip()] = value.strip()
    if not saw_entry:
        raise VerificationError("desktop entry is missing [Desktop Entry]")
    if fields.get("Type") != "Application":
        raise VerificationError(
            f"desktop entry Type must be Application, got {fields.get('Type')!r}"
        )
    argv0 = _desktop_argv0(fields.get("Exec", ""))
    if argv0.split("/")[-1] != "snip-desktop-native":
        raise VerificationError(
            "desktop entry Exec must launch snip-desktop-native, "
            f"got {fields.get('Exec', '')!r}"
        )
    if fields.get("Version") != "1.0":
        raise VerificationError(
            "desktop entry Version must be the Desktop Entry spec version 1.0, "
            f"got {fields.get('Version')!r}"
        )


def _add_unique(found: Dict[str, str], key: str, name: str, label: str, where: str) -> None:
    if key in found:
        raise VerificationError(
            f"Ambiguous multiple {label} metadata entries in {where}: {name}, {found[key]}"
        )
    found[key] = name


def _norm_arc_name(name: str) -> str:
    normalized = name.replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized


def _basename(name: str) -> str:
    return name.rstrip("/").split("/")[-1]


def _bind_package_members(
    *,
    os_kind: Optional[str],
    binary_name: str,
    binary_paths: Sequence[str],
    readmes: Sequence[str],
    plists: Sequence[str],
    desktops: Sequence[str],
    expected_version: Optional[str],
    seen: Sequence[str],
    where: str,
) -> Dict[str, str]:
    """Bind the single executable to the metadata in the same package_native layout."""
    if len(binary_paths) == 0:
        raise VerificationError(
            f"Expected executable '{binary_name}' not found in {where}. Members: {list(seen)}"
        )
    if len(binary_paths) > 1:
        raise VerificationError(
            f"Multiple/ambiguous executable members found matching '{binary_name}': {list(binary_paths)}"
        )
    binary = binary_paths[0]
    kind = _layout_kind(binary, binary_name)
    if os_kind is None:
        os_kind = kind
    if kind is None or os_kind is None or kind != os_kind:
        raise VerificationError(
            f"Binary path '{binary}' does not match the required {os_kind or 'candidate'} package layout"
        )
    bound: Dict[str, str] = {"binary": binary, "os": os_kind}
    if os_kind == "macos":
        if list(plists) != [MAC_PLIST]:
            raise VerificationError(
                "macOS Info.plist must be the same bundle's "
                f"{MAC_PLIST}; found {list(plists)}"
            )
        bound["plist"] = MAC_PLIST
        return bound
    if os_kind == "linux":
        package = _linux_package_name(binary, binary_name)
        accepted = _accepted_linux_packages(expected_version)
        if package is None or (accepted is not None and package not in accepted):
            raise VerificationError(
                f"Linux package directory '{package}' does not match expected version "
                f"'{_version_token(expected_version or '')}'"
            )
        readme = f"{package}/README.txt"
        desktop = f"{package}/share/applications/{DESKTOP_FILE_NAME}"
        if list(readmes) != [readme]:
            raise VerificationError(
                f"Linux README must be {readme} in the same package as the binary; found {list(readmes)}"
            )
        if list(desktops) != [desktop]:
            raise VerificationError(
                f"Linux desktop entry must be {desktop}; found {list(desktops)}"
            )
        if plists:
            raise VerificationError(
                f"Linux package contains disconnected Info.plist metadata: {list(plists)}"
            )
        bound["readme"] = readme
        bound["desktop"] = desktop
        bound["package"] = package
        return bound
    if list(readmes) != [WIN_README]:
        raise VerificationError(
            f"Windows README must be {WIN_README} in the same package as the executable; "
            f"found {list(readmes)}"
        )
    if plists or desktops:
        raise VerificationError(
            "Windows package contains disconnected metadata: "
            f"plists={list(plists)} desktops={list(desktops)}"
        )
    bound["readme"] = WIN_README
    return bound


def _licenses_dir(bound: Dict[str, str]) -> str:
    if bound["os"] == "macos":
        return MAC_LICENSES_DIR
    if bound["os"] == "linux":
        return f"{bound['package']}/{LICENSES_DIR_NAME}"
    return WIN_LICENSES_DIR


def _require_licenses(sizes: Dict[str, int], lic_dir: str, where: str) -> None:
    """Require exactly LICENSE_FILES, non-empty, as the only files in any licenses/ folder."""
    found = sorted(
        name for name in sizes if LICENSES_DIR_NAME in name.split("/")[:-1]
    )
    wanted = sorted(f"{lic_dir}/{name}" for name in LICENSE_FILES)
    if found != wanted:
        raise VerificationError(
            f"License files in {where} must be exactly {wanted}; found {found}"
        )
    empty = [name for name in found if sizes[name] <= 0]
    if empty:
        raise VerificationError(f"Empty license files in {where}: {empty}")


def _read_exact(stream: BinaryIO, size: int, label: str) -> bytes:
    if size < 0 or size > MAX_METADATA_BYTES:
        raise VerificationError(f"{label} exceeds {MAX_METADATA_BYTES} byte metadata bound")
    data = stream.read(size + 1)
    if len(data) != size:
        raise VerificationError(f"{label} short read: expected {size} bytes, got {len(data)}")
    return data


def _check_exec_mode(mode: int, label: str) -> None:
    perm = mode & 0o7777
    if not (perm & 0o111):
        raise VerificationError(f"Binary '{label}' lacks execute bits (mode: {oct(mode)})")
    if perm & 0o7000:
        raise VerificationError(f"Binary '{label}' has special permission bits (mode: {oct(mode)})")


def verify_macos_bundle(
    bundle_path: Path,
    expected_version: Optional[str] = None,
    expected_arch: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Verifies a macOS .app bundle on disk.
    """
    if bundle_path.is_symlink() or not bundle_path.is_dir() or not bundle_path.name.endswith(".app"):
        raise VerificationError(f"Invalid macOS app bundle directory: {bundle_path}")

    contents = bundle_path / "Contents"
    plist_path = contents / "Info.plist"
    if plist_path.is_symlink() or not plist_path.is_file():
        raise VerificationError(f"Missing Info.plist in {bundle_path}")

    plist_size = plist_path.stat().st_size
    if plist_size > MAX_METADATA_BYTES:
        raise VerificationError(f"Info.plist exceeds {MAX_METADATA_BYTES} byte metadata bound")
    with plist_path.open("rb") as handle:
        plist = _load_plist(handle.read())

    short_version = _validate_plist(plist, expected_version)
    exec_name = plist.get("CFBundleExecutable")
    exec_path = contents / "MacOS" / str(exec_name)
    if exec_path.is_symlink() or not exec_path.is_file():
        raise VerificationError(f"Missing main executable at {exec_path}")

    if os.name == "posix" and not os.access(exec_path, os.X_OK):
        raise VerificationError(f"Executable {exec_path} lacks execute permissions")

    bin_info = detect_binary_format_and_arch(exec_path)
    _require_format(bin_info, "macho", str(exec_path))
    _require_arch(bin_info, expected_arch, str(exec_path))
    _require_licenses(
        {
            f"{MAC_LICENSES_DIR}/{f.name}": f.stat().st_size
            for f in (contents / "Resources" / LICENSES_DIR_NAME).glob("*")
            if not f.is_symlink() and f.is_file()
        },
        MAC_LICENSES_DIR,
        str(bundle_path),
    )

    # On macOS: verify ad-hoc code signature
    signature_info = {"verified": False, "adhoc": False}
    if platform.system() == "Darwin":
        res = subprocess.run(
            ["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(bundle_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        if res.returncode != 0:
            raise VerificationError(f"codesign verification failed for {bundle_path}:\n{res.stderr}\n{res.stdout}")
        signature_info["verified"] = True

        disp = subprocess.run(
            ["codesign", "-d", "--verbose=2", str(bundle_path)],
            capture_output=True,
            text=True,
            check=False,
        )
        sig_text = disp.stderr + disp.stdout
        signature_info["adhoc"] = "Signature=adhoc" in sig_text or "Authority=" not in sig_text

    return {
        "bundle": str(bundle_path),
        "executable": str(exec_path),
        "arch": bin_info["arch"],
        "version": short_version,
        "signature": signature_info,
    }


def verify_dmg(
    dmg_path: Path,
    expected_version: Optional[str] = None,
    expected_arch: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Verifies a macOS DMG disk image on Darwin:
    attaches read-only, verifies exactly one .app and the /Applications symlink,
    audits bundle structure, version, arch, and signature, then detaches.
    Detach failures raise. Non-Darwin hosts cannot complete this audit.
    """
    if dmg_path.is_symlink() or not dmg_path.is_file():
        raise VerificationError(f"DMG file not found: {dmg_path}")
    if dmg_path.stat().st_size < 1024:
        raise VerificationError(f"DMG file suspiciously small ({dmg_path.stat().st_size} bytes): {dmg_path}")

    if platform.system() != "Darwin":
        raise VerificationError(
            f"DMG {dmg_path.name} cannot be fully audited on non-Darwin host "
            f"({platform.system()}): hdiutil mount, codesign, and detach were not run. "
            "A structural archive check is not a passed Apple audit."
        )

    result: Dict[str, Any] = {"dmg": str(dmg_path), "verified_on_darwin": False, "audit_scope": "full"}

    if platform.system() == "Darwin":
        mnt = Path(tempfile.mkdtemp(prefix="snip_dmg_mnt_"))
        try:
            attach_res = subprocess.run(
                ["hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", str(mnt), str(dmg_path)],
                capture_output=True,
                text=True,
                check=False,
            )
            if attach_res.returncode != 0:
                raise VerificationError(f"hdiutil attach failed for {dmg_path}:\n{attach_res.stderr}")

            # Check .app inside DMG
            apps = list(mnt.glob("*.app"))
            if len(apps) != 1:
                raise VerificationError(f"Expected exactly 1 .app in DMG, found {len(apps)}: {apps}")

            bundle_info = verify_macos_bundle(apps[0], expected_version=expected_version, expected_arch=expected_arch)
            result["bundle"] = bundle_info

            # Verify /Applications symlink presence and exact target
            app_link = mnt / "Applications"
            if not app_link.is_symlink():
                raise VerificationError(f"Missing Applications symlink in DMG root: {dmg_path}")
            link_target = os.readlink(str(app_link))
            if link_target != "/Applications":
                raise VerificationError(
                    f"Applications symlink in DMG points to '{link_target}', expected '/Applications'"
                )

        finally:
            detach_res = subprocess.run(
                ["hdiutil", "detach", str(mnt)],
                capture_output=True,
                text=True,
                check=False,
            )
            try:
                mnt.rmdir()
            except Exception:
                pass
            if detach_res.returncode != 0:
                raise VerificationError(
                    f"hdiutil detach failed for mount {mnt} ({dmg_path}):\n{detach_res.stderr}"
                )

    result["verified_on_darwin"] = True
    return result


def _os_kind_from_format(expected_format: Optional[str], binary_name: str) -> Optional[str]:
    if expected_format == "elf":
        return "linux"
    if expected_format == "pe" or binary_name.endswith(".exe"):
        return "windows"
    if expected_format == "macho":
        return "macos"
    return None


def _stream_for_tar(tf: tarfile.TarFile, member: tarfile.TarInfo) -> BinaryIO:
    handle = tf.extractfile(member)
    if handle is None:
        raise VerificationError(f"Cannot read archive member {member.name}")
    return handle


def _inspect_tar_member(tf: tarfile.TarFile, member: tarfile.TarInfo) -> Dict[str, Any]:
    if member.size > MAX_BINARY_BYTES:
        raise VerificationError(
            f"Binary member {member.name} size {member.size} exceeds {MAX_BINARY_BYTES} byte bound"
        )
    with _stream_for_tar(tf, member) as handle:
        return inspect_binary_stream(handle, member.size)


def _read_tar_metadata(tf: tarfile.TarFile, member: tarfile.TarInfo) -> bytes:
    if member.size > MAX_METADATA_BYTES:
        raise VerificationError(
            f"{member.name} exceeds {MAX_METADATA_BYTES} byte metadata bound"
        )
    with _stream_for_tar(tf, member) as handle:
        return _read_exact(handle, member.size, member.name)


def _validate_bound_metadata(
    tf: tarfile.TarFile,
    by_name: Dict[str, tarfile.TarInfo],
    bound: Dict[str, str],
    expected_version: Optional[str],
) -> None:
    if bound["os"] == "macos":
        plist = _load_plist(_read_tar_metadata(tf, by_name[bound["plist"]]))
        _validate_plist(plist, expected_version)
        return
    if bound["os"] == "linux":
        desktop = _read_tar_metadata(tf, by_name[bound["desktop"]]).decode("utf-8", errors="replace")
        _validate_desktop(desktop)
        if expected_version:
            readme = _read_tar_metadata(tf, by_name[bound["readme"]]).decode("utf-8", errors="replace")
            _validate_readme(readme, expected_version, bound["readme"])
        return
    if expected_version:
        readme = _read_tar_metadata(tf, by_name[bound["readme"]]).decode("utf-8", errors="replace")
        _validate_readme(readme, expected_version, bound["readme"])


def verify_tar_archive(
    tar_path: Path,
    expected_binary_name: Optional[str] = None,
    expected_format: Optional[str] = None,
    expected_arch: Optional[str] = None,
    expected_version: Optional[str] = None,
    target: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Safely verifies a Linux or macOS tarball without extracting it to disk.
    The binary must sit on the package_native path, and version metadata must be
    that same package's Info.plist or README. This is a structural check
    (audit_scope=structural), not a full Apple publication audit.
    """
    if not tar_path.is_file():
        raise VerificationError(f"Tar archive not found: {tar_path}")

    resolved = resolve_verification_constraints(
        target,
        arch=expected_arch,
        binary_format=expected_format,
        binary_name=expected_binary_name,
        artifact_kind="tar" if target else None,
    )
    if resolved["target"]:
        expected_binary_name = resolved["binary_name"]
        expected_format = resolved["format"]
        expected_arch = resolved["arch"]
        os_kind: Optional[str] = resolved["os"]
    else:
        expected_binary_name = expected_binary_name or "snip-desktop-native"
        expected_format = resolved["format"]
        expected_arch = resolved["arch"]
        os_kind = _os_kind_from_format(expected_format, expected_binary_name)

    with tarfile.open(tar_path, "r:*") as tf:
        seen_names = set()
        binary_paths: List[str] = []
        meta: Dict[str, str] = {}
        by_name: Dict[str, tarfile.TarInfo] = {}

        for member in tf.getmembers():
            check_safe_archive_path(member.name)
            name = _norm_arc_name(member.name)
            if name in seen_names:
                raise VerificationError(f"Duplicate entry in archive: {member.name}")
            seen_names.add(name)
            if member.issym() or member.islnk():
                raise VerificationError(
                    f"Symlinks not permitted in candidate tarball: {member.name} -> {member.linkname}"
                )
            if member.isdir():
                continue
            if not member.isreg():
                raise VerificationError(f"Non-regular member not permitted in candidate tarball: {member.name}")
            by_name[name] = member
            base = _basename(name)
            if base == expected_binary_name:
                binary_paths.append(name)
            elif base == "README.txt":
                _add_unique(meta, "readme", name, "README.txt", "archive")
            elif base == "Info.plist":
                _add_unique(meta, "plist", name, "Info.plist", "archive")
            elif base == DESKTOP_FILE_NAME:
                _add_unique(meta, "desktop", name, DESKTOP_FILE_NAME, "archive")

        bound = _bind_package_members(
            os_kind=os_kind,
            binary_name=expected_binary_name,
            binary_paths=binary_paths,
            readmes=[meta["readme"]] if "readme" in meta else [],
            plists=[meta["plist"]] if "plist" in meta else [],
            desktops=[meta["desktop"]] if "desktop" in meta else [],
            expected_version=expected_version,
            seen=sorted(seen_names),
            where=f"archive {tar_path}",
        )
        target_member = by_name[bound["binary"]]
        _check_exec_mode(target_member.mode, target_member.name)
        _validate_bound_metadata(tf, by_name, bound, expected_version)
        bin_info = _inspect_tar_member(tf, target_member)
        _require_format(bin_info, expected_format, str(tar_path))
        _require_arch(bin_info, expected_arch, str(tar_path))
        _require_licenses(
            {name: m.size for name, m in by_name.items()},
            _licenses_dir(bound),
            f"archive {tar_path}",
        )

    return {
        "archive": str(tar_path),
        "binary": target_member.name,
        "format": bin_info["format"],
        "arch": bin_info["arch"],
        "slices": list(bin_info.get("slices") or []),
        "is_64bit": bin_info["is_64bit"],
        "audit_scope": "structural",
    }


def _read_zip_metadata(zf: zipfile.ZipFile, info: zipfile.ZipInfo) -> bytes:
    if info.file_size > MAX_METADATA_BYTES:
        raise VerificationError(
            f"{info.filename} exceeds {MAX_METADATA_BYTES} byte metadata bound"
        )
    with zf.open(info) as handle:
        return _read_exact(handle, info.file_size, info.filename)


def _validate_zip_metadata(
    zf: zipfile.ZipFile,
    by_name: Dict[str, zipfile.ZipInfo],
    bound: Dict[str, str],
    expected_version: Optional[str],
) -> None:
    if bound["os"] == "macos":
        plist = _load_plist(_read_zip_metadata(zf, by_name[bound["plist"]]))
        _validate_plist(plist, expected_version)
        return
    if bound["os"] == "linux":
        desktop = _read_zip_metadata(zf, by_name[bound["desktop"]]).decode("utf-8", errors="replace")
        _validate_desktop(desktop)
        if expected_version:
            readme = _read_zip_metadata(zf, by_name[bound["readme"]]).decode("utf-8", errors="replace")
            _validate_readme(readme, expected_version, bound["readme"])
        return
    if expected_version:
        readme = _read_zip_metadata(zf, by_name[bound["readme"]]).decode("utf-8", errors="replace")
        _validate_readme(readme, expected_version, bound["readme"])


def verify_zip_archive(
    zip_path: Path,
    expected_binary_name: Optional[str] = None,
    expected_format: Optional[str] = None,
    expected_arch: Optional[str] = None,
    expected_version: Optional[str] = None,
    target: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Safely verifies a Windows zip package without extracting it to disk.
    The executable and README must share the package_native directory.
    This is a structural check, not a full Apple publication audit.
    """
    if not zip_path.is_file():
        raise VerificationError(f"Zip archive not found: {zip_path}")

    resolved = resolve_verification_constraints(
        target,
        arch=expected_arch,
        binary_format=expected_format,
        binary_name=expected_binary_name,
        artifact_kind="zip" if target else None,
    )
    if resolved["target"]:
        expected_binary_name = resolved["binary_name"]
        expected_format = resolved["format"]
        expected_arch = resolved["arch"]
        os_kind = resolved["os"]
    else:
        expected_binary_name = expected_binary_name or "snip-desktop-native.exe"
        expected_format = resolved["format"]
        expected_arch = resolved["arch"]
        os_kind = _os_kind_from_format(expected_format, expected_binary_name)

    with open(zip_path, "rb") as fp:
        with zipfile.ZipFile(fp, "r") as zf:
            seen_names = set()
            binary_paths: List[str] = []
            meta: Dict[str, str] = {}
            by_name: Dict[str, zipfile.ZipInfo] = {}

            for info in zf.infolist():
                check_safe_archive_path(info.filename)
                name = _norm_arc_name(info.filename)
                if name in seen_names:
                    raise VerificationError(f"Duplicate entry in zip: {info.filename}")
                seen_names.add(name)
                if info.is_dir():
                    continue
                mode = info.external_attr >> 16
                ftype = mode & 0o170000
                if ftype == 0o120000:
                    raise VerificationError(f"Symlinks not permitted in candidate zip: {info.filename}")
                if ftype != 0 and ftype not in (0o100000, 0o040000):
                    raise VerificationError(
                        f"Unexpected non-regular file type in zip (type {oct(ftype)}): {info.filename}"
                    )
                by_name[name] = info
                base = _basename(name)
                if base == expected_binary_name:
                    binary_paths.append(name)
                elif base == "README.txt":
                    _add_unique(meta, "readme", name, "README.txt", "zip")
                elif base == "Info.plist":
                    _add_unique(meta, "plist", name, "Info.plist", "zip")
                elif base == DESKTOP_FILE_NAME:
                    _add_unique(meta, "desktop", name, DESKTOP_FILE_NAME, "zip")

            bound = _bind_package_members(
                os_kind=os_kind,
                binary_name=expected_binary_name,
                binary_paths=binary_paths,
                readmes=[meta["readme"]] if "readme" in meta else [],
                plists=[meta["plist"]] if "plist" in meta else [],
                desktops=[meta["desktop"]] if "desktop" in meta else [],
                expected_version=expected_version,
                seen=sorted(seen_names),
                where=f"zip {zip_path}",
            )
            target_info = by_name[bound["binary"]]
            if target_info.file_size > MAX_BINARY_BYTES:
                raise VerificationError(
                    f"Binary member {target_info.filename} exceeds {MAX_BINARY_BYTES} byte bound"
                )
            _validate_zip_metadata(zf, by_name, bound, expected_version)
            with zf.open(target_info) as handle:
                bin_info = inspect_binary_stream(handle, target_info.file_size)
            _require_format(bin_info, expected_format, str(zip_path))
            _require_arch(bin_info, expected_arch, str(zip_path))
            _require_licenses(
                {name: i.file_size for name, i in by_name.items()},
                _licenses_dir(bound),
                f"zip {zip_path}",
            )

    return {
        "archive": str(zip_path),
        "binary": target_info.filename,
        "format": bin_info["format"],
        "arch": bin_info["arch"],
        "slices": list(bin_info.get("slices") or []),
        "is_64bit": bin_info["is_64bit"],
        "audit_scope": "structural",
    }


def verify_executable_binary(
    binary_path: Path,
    expected_arch: Optional[str] = None,
    expected_format: Optional[str] = None,
) -> Dict[str, Any]:
    """Verifies a standalone executable binary file directly."""
    if binary_path.is_symlink() or not binary_path.is_file():
        raise VerificationError(f"Binary not found: {binary_path}")

    if os.name == "posix" and not os.access(binary_path, os.X_OK):
        raise VerificationError(f"Binary is not executable: {binary_path}")

    bin_info = detect_binary_format_and_arch(binary_path)
    if bin_info["format"] == "unknown":
        raise VerificationError(f"Unrecognized binary format for: {binary_path}")

    _require_format(bin_info, expected_format, str(binary_path))
    _require_arch(bin_info, expected_arch, str(binary_path))

    return {
        "path": str(binary_path),
        "format": bin_info["format"],
        "arch": bin_info["arch"],
        "is_64bit": bin_info["is_64bit"],
        "sha256": compute_sha256(binary_path),
    }


def _required_candidate_paths(directory: Path, target: str) -> List[Path]:
    required = list(CANDIDATE_FILES[target])
    files: List[str] = []
    dirs: List[str] = []
    for entry in directory.iterdir():
        if entry.is_symlink():
            raise VerificationError(f"Symlink not permitted in artifact directory: {entry.name}")
        if entry.is_dir():
            dirs.append(entry.name)
        elif entry.is_file() and not entry.name.startswith("SHA256SUMS"):
            files.append(entry.name)
    missing = [name for name in required if name not in files]
    extra = [name for name in files if name not in required]
    if missing or extra or dirs:
        raise VerificationError(
            "Candidate artifact set for "
            f"{target} is incomplete or unexpected: "
            f"missing={missing}, unexpected_files={extra}, unexpected_dirs={dirs}"
        )
    return [directory / name for name in required]


def _verify_candidate_file(
    path: Path,
    *,
    expected_binary: Optional[str],
    expected_format: Optional[str],
    expected_arch: Optional[str],
    expected_version: Optional[str],
    target: str,
) -> Dict[str, Any]:
    if path.name.endswith(".tar.gz"):
        info = verify_tar_archive(
            path,
            expected_binary_name=expected_binary,
            expected_format=expected_format,
            expected_arch=expected_arch,
            expected_version=expected_version,
            target=target,
        )
        return {"type": "tarball", **info}
    if path.name.endswith(".dmg"):
        info = verify_dmg(path, expected_version=expected_version, expected_arch=expected_arch)
        return {"type": "macos_dmg", **info}
    if path.name.endswith(".zip"):
        info = verify_zip_archive(
            path,
            expected_binary_name=expected_binary,
            expected_format=expected_format,
            expected_arch=expected_arch,
            expected_version=expected_version,
            target=target,
        )
        return {"type": "zip", **info}
    if path.name.endswith("-setup.exe"):
        return {"type": "windows_installer", **verify_windows_installer(path)}
    raise VerificationError(f"No verifier for candidate file {path.name}")


def verify_windows_installer(path: Path) -> Dict[str, Any]:
    """
    Structural check of the Inno Setup installer: a well-formed PE of plausible size.
    The setup stub is a 32-bit x86 PE regardless of the payload, so no arch check.
    The payload is bound to the packaged exe by the CI install-and-hash step.
    """
    if path.is_symlink() or not path.is_file():
        raise VerificationError(f"Installer not found: {path}")
    size = path.stat().st_size
    if size < 1024 * 1024:
        raise VerificationError(f"Installer suspiciously small ({size} bytes): {path}")
    bin_info = detect_binary_format_and_arch(path)
    _require_format(bin_info, "pe", str(path))
    return {"installer": str(path), "format": bin_info["format"], "stub_arch": bin_info["arch"]}


def audit_artifact_directory(
    directory: Path,
    expected_version: Optional[str] = None,
    target_arch: Optional[str] = None,
    target: Optional[str] = None,
    require_checksums: bool = True,
) -> Dict[str, Any]:
    """
    Audits all candidate package artifacts in a directory.
    """
    if not directory.is_dir():
        raise VerificationError(f"Artifact directory does not exist: {directory}")

    resolved = resolve_verification_constraints(target, arch=target_arch)
    expected_format = resolved["format"]
    expected_binary = resolved["binary_name"]
    target_arch = resolved["arch"]

    results: Dict[str, Any] = {
        "directory": str(directory),
        "target": target,
        "artifacts_checked": 0,
        "details": [],
        "full_audit": False,
    }

    if target:
        for path in _required_candidate_paths(directory, target):
            info = _verify_candidate_file(
                path,
                expected_binary=expected_binary,
                expected_format=expected_format,
                expected_arch=target_arch,
                expected_version=expected_version,
                target=target,
            )
            if path.name.endswith(".dmg") and info.get("verified_on_darwin") is not True:
                raise VerificationError(
                    f"DMG {path.name} was not verified on Darwin; full Apple audit cannot pass"
                )
            results["details"].append(info)
            results["artifacts_checked"] += 1
    else:
        for app in sorted(directory.glob("*.app")):
            info = verify_macos_bundle(app, expected_version=expected_version, expected_arch=target_arch)
            results["details"].append({"type": "macos_app", **info})
            results["artifacts_checked"] += 1
        for dmg in sorted(directory.glob("*.dmg")):
            info = verify_dmg(dmg, expected_version=expected_version, expected_arch=target_arch)
            if info.get("verified_on_darwin") is not True:
                raise VerificationError(
                    f"DMG {dmg.name} was not verified on Darwin; full Apple audit cannot pass"
                )
            results["details"].append({"type": "macos_dmg", **info})
            results["artifacts_checked"] += 1
        for tar in sorted(directory.glob("*.tar.gz")):
            info = verify_tar_archive(
                tar,
                expected_binary_name=expected_binary,
                expected_format=expected_format,
                expected_arch=target_arch,
                expected_version=expected_version,
                target=target,
            )
            results["details"].append({"type": "tarball", **info})
            results["artifacts_checked"] += 1
        for zp in sorted(directory.glob("*.zip")):
            info = verify_zip_archive(
                zp,
                expected_binary_name=expected_binary,
                expected_format=expected_format,
                expected_arch=target_arch,
                expected_version=expected_version,
                target=target,
            )
            results["details"].append({"type": "zip", **info})
            results["artifacts_checked"] += 1

    sums_files = sorted(directory.glob("SHA256SUMS*.txt"))
    if require_checksums and not sums_files:
        raise VerificationError(f"Missing required SHA256SUMS file in {directory}")

    for sf in sums_files:
        checksum_results = verify_checksums_file(sf, require_complete_coverage=True)
        failures = [r for r in checksum_results if not r[1]]
        if failures:
            fail_msg = "\n".join(f"  - {f[0]}: {f[2]}" for f in failures)
            raise VerificationError(f"Checksum verification failed in {sf.name}:\n{fail_msg}")
        results["checksums_verified"] = len(checksum_results)

    if results["artifacts_checked"] == 0:
        raise VerificationError(f"No recognizable artifacts (*.app, *.dmg, *.tar.gz, *.zip) found in {directory}")

    # A targeted candidate set is the publication audit. Untargeted scans stay structural.
    results["full_audit"] = target is not None
    if target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
        if not any(item.get("verified_on_darwin") is True for item in results["details"]):
            raise VerificationError(
                f"Apple candidate audit for {target} did not record a Darwin DMG verification"
            )
    return results


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify native cross-platform build artifacts and packages."
    )
    parser.add_argument(
        "--dir",
        "-d",
        type=Path,
        default=None,
        help="Directory containing artifacts to inspect.",
    )
    parser.add_argument(
        "--file",
        "-f",
        type=Path,
        default=None,
        help="Single artifact or binary file to inspect.",
    )
    parser.add_argument(
        "--version",
        "-v",
        default=None,
        help="Expected version (e.g. 0.1.4 or v0.1.4).",
    )
    parser.add_argument(
        "--arch",
        "-a",
        default=None,
        help="Expected architecture (e.g. x86_64, aarch64, arm64).",
    )
    parser.add_argument(
        "--target",
        "-t",
        default=None,
        help="Target triple (e.g. x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin, x86_64-apple-darwin).",
    )
    parser.add_argument(
        "--require-checksums",
        action="store_true",
        help="Require SHA256SUMS file to be present with complete coverage.",
    )
    parser.add_argument(
        "--generate-checksums",
        action="store_true",
        help="Generate SHA256SUMS file in target directory.",
    )
    parser.add_argument(
        "--target-triple",
        default=None,
        help="Target triple for naming target-specific checksum files.",
    )
    return parser.parse_args(argv)


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])

    try:
        if args.generate_checksums:
            if not args.dir:
                print("Error: --dir required with --generate-checksums", file=sys.stderr)
                return 1
            triple = args.target_triple or args.target
            sums_path = generate_checksums(args.dir, target_triple=triple)
            print(f"Generated checksums file: {sums_path}")
            return 0

        if args.file:
            p = args.file
            kind = _file_artifact_kind(p)
            resolved = resolve_verification_constraints(
                args.target,
                arch=args.arch,
                artifact_kind=kind if args.target else None,
            )
            if kind == "app":
                res = verify_macos_bundle(p, expected_version=args.version, expected_arch=resolved["arch"])
            elif kind == "dmg":
                res = verify_dmg(p, expected_version=args.version, expected_arch=resolved["arch"])
            elif kind == "tar":
                res = verify_tar_archive(
                    p,
                    expected_binary_name=resolved["binary_name"],
                    expected_format=resolved["format"],
                    expected_arch=resolved["arch"],
                    expected_version=args.version,
                    target=args.target,
                )
            elif kind == "zip":
                res = verify_zip_archive(
                    p,
                    expected_binary_name=resolved["binary_name"],
                    expected_format=resolved["format"],
                    expected_arch=resolved["arch"],
                    expected_version=args.version,
                    target=args.target,
                )
            else:
                res = verify_executable_binary(
                    p,
                    expected_arch=resolved["arch"],
                    expected_format=resolved["format"],
                )
            if isinstance(res, dict) and res.get("audit_scope") == "structural":
                print(f"Structural check only (not a full platform audit) for {p}: {res}")
            else:
                print(f"Verified {p}: {res}")
            return 0

        if args.dir:
            res = audit_artifact_directory(
                args.dir,
                expected_version=args.version,
                target_arch=args.arch,
                target=args.target,
                require_checksums=args.require_checksums,
            )
            if not res.get("full_audit"):
                print(
                    f"Structural check only (not a full platform audit) for {args.dir}: "
                    f"{res['artifacts_checked']} artifact(s) inspected.",
                    file=sys.stderr,
                )
                return 1
            print(f"Artifact audit passed for {args.dir}: {res['artifacts_checked']} artifact(s) verified.")
            return 0

        print("Error: Specify either --dir, --file, or --generate-checksums", file=sys.stderr)
        return 1

    except VerificationError as e:
        print(f"Artifact Verification Error: {e}", file=sys.stderr)
        return 1
    except Exception as e:
        print(f"Unexpected error during verification: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
