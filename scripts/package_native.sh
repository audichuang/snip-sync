#!/usr/bin/env bash
# scripts/package_native.sh
# Strict native desktop packager using platform tools. Its outputs are the
# release assets: release.yml republishes them byte for byte (see
# docs/native-cross-platform-ci-and-packaging.md). Per-target checksums:
#   - macOS: snip-sync.app (ad-hoc signed) in snip-sync_mac_<arch>.dmg with an
#     /Applications link, plus snip-sync_mac_<arch>.app.tar.gz
#   - Linux: snip-sync-linux-x86_64.tar.gz with executable, .desktop entry, README
#   Every package also carries licenses/ (third-party font and icon licenses).
#   - Windows: snip-sync-windows-x64.zip and the Inno Setup installer
#     snip-sync-windows-setup.exe (per-user, unsigned)
#
# Usage:
#   scripts/package_native.sh <TARGET_TRIPLE> <OUTPUT_DIR> <BINARY_PATH> <VERSION>

set -euo pipefail

if [ "$#" -lt 4 ]; then
    echo "Usage: $0 <TARGET_TRIPLE> <OUTPUT_DIR> <BINARY_PATH> <VERSION>" >&2
    echo "Error: All 4 arguments are mandatory. No default fallbacks allowed." >&2
    exit 1
fi

TARGET="$1"
OUT_DIR_INPUT="$2"
BIN_PATH="$3"
VERSION="$4"

if [ -z "$VERSION" ]; then
    echo "Error: VERSION cannot be empty." >&2
    exit 1
fi

if [ ! -f "$BIN_PATH" ]; then
    echo "Error: Binary not found at '$BIN_PATH'." >&2
    exit 1
fi

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Error: required tool '$1' not found; cannot package target '$TARGET'." >&2
        exit 1
    fi
}

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# The Tauri crate stays in the tree for rollback; its icons are the product icons.
ICON_DIR="$ROOT_DIR/crates/desktop/src-tauri/icons"

# Third-party licenses for what the binary embeds: the OFL covers every Inter
# and JetBrains Mono face (Regular, SemiBold, Italic, ...), Apache-2.0 the
# IntelliJ expui icons. scripts/verify_artifacts.py requires exactly this set.
stage_licenses() {
    mkdir -p "$1"
    cp "$ROOT_DIR/crates/desktop-native/assets/fonts/Inter-OFL.txt" "$1/Inter-OFL.txt"
    cp "$ROOT_DIR/crates/desktop-native/assets/fonts/JetBrainsMono-OFL.txt" "$1/JetBrainsMono-OFL.txt"
    cp "$ROOT_DIR/crates/desktop-native/assets/icons/LICENSE.txt" "$1/expui-icons-LICENSE.txt"
    cp "$ROOT_DIR/crates/desktop-native/assets/icons/NOTICE.txt" "$1/expui-icons-NOTICE.txt"
}

# Darwin always gets an ad-hoc signature and a DMG, Windows always an installer.
# Missing tools must fail the target instead of omitting that format.
case "$TARGET" in
    aarch64-apple-darwin|x86_64-apple-darwin)
        require_tool codesign
        require_tool hdiutil
        ;;
    x86_64-pc-windows-msvc)
        # Preinstalled on GitHub's Windows images, but not on PATH.
        ISCC="$(command -v iscc || true)"
        if [ -z "$ISCC" ] && [ -x "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" ]; then
            ISCC="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"
        fi
        if [ -z "$ISCC" ]; then
            echo "Error: Inno Setup compiler (ISCC.exe) not found; cannot package target '$TARGET'." >&2
            exit 1
        fi
        require_tool cygpath
        ;;
esac

# Ensure output directory exists and resolve to absolute path (critical for cd in sub-steps)
mkdir -p "$OUT_DIR_INPUT"
OUT_DIR="$(cd "$OUT_DIR_INPUT" && pwd)"

echo "=== Native Desktop Packager ==="
echo "Target:     $TARGET"
echo "Version:    $VERSION"
echo "Binary:     $BIN_PATH"
echo "Output dir: $OUT_DIR"

STAGE_DIR=$(mktemp -d)
trap 'rm -rf "$STAGE_DIR"' EXIT

case "$TARGET" in
    aarch64-apple-darwin|x86_64-apple-darwin)
        APP_NAME="snip-sync.app"
        APP_DIR="$STAGE_DIR/$APP_NAME"
        CONTENTS_DIR="$APP_DIR/Contents"
        MACOS_DIR="$CONTENTS_DIR/MacOS"
        RESOURCES_DIR="$CONTENTS_DIR/Resources"

        mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

        cp "$BIN_PATH" "$MACOS_DIR/snip-desktop-native"
        chmod 755 "$MACOS_DIR/snip-desktop-native"
        cp "$ICON_DIR/icon.icns" "$RESOURCES_DIR/icon.icns"
        # Inside Resources so the code signature seals them.
        stage_licenses "$RESOURCES_DIR/licenses"

        # Explicit macOS deployment target requirement (macOS 11.0 Big Sur)
        # Derived from Rust tier 1 Apple Silicon and GPUI Metal backend requirements.
        MIN_OS="11.0"
        if [ "$TARGET" = "aarch64-apple-darwin" ]; then
            ARCH_LABEL="mac_arm"
        else
            ARCH_LABEL="mac_intel"
        fi

        # Generate standard Info.plist
        cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>snip-desktop-native</string>
    <key>CFBundleIdentifier</key>
    <string>com.audichuang.snip-sync</string>
    <key>CFBundleName</key>
    <string>snip-sync</string>
    <key>CFBundleDisplayName</key>
    <string>snip-sync</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>${MIN_OS}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

        echo "Applying ad-hoc code signature to $APP_NAME..."
        codesign --force --deep --sign - "$APP_DIR"
        echo "Verifying code signature..."
        codesign --verify --deep --strict --verbose=2 "$APP_DIR"

        # 1. Tarball containing the .app bundle
        TAR_OUT="$OUT_DIR/snip-sync_${ARCH_LABEL}.app.tar.gz"
        tar -czf "$TAR_OUT" -C "$STAGE_DIR" "$APP_NAME"
        echo "Created archive: $TAR_OUT"

        # 2. DMG with standard drag-to-Applications symlink
        DMG_STAGE="$STAGE_DIR/dmg_root"
        mkdir -p "$DMG_STAGE"
        cp -R "$APP_DIR" "$DMG_STAGE/"
        ln -s /Applications "$DMG_STAGE/Applications"

        DMG_OUT="$OUT_DIR/snip-sync_${ARCH_LABEL}.dmg"
        echo "Creating DMG volume from stage directory..."
        hdiutil create -volname "snip-sync" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG_OUT"
        echo "Created DMG: $DMG_OUT"
        ;;

    x86_64-pc-windows-msvc)
        ZIP_OUT="$OUT_DIR/snip-sync-windows-x64.zip"
        PKG_DIR="$STAGE_DIR/snip-sync"
        mkdir -p "$PKG_DIR"

        cp "$BIN_PATH" "$PKG_DIR/snip-desktop-native.exe"
        stage_licenses "$PKG_DIR/licenses"

        cat > "$PKG_DIR/README.txt" <<README
snip-sync ${VERSION} (native desktop app, unsigned)
Version: ${VERSION}
Target: ${TARGET}

Usage:
  snip-desktop-native.exe --help
  snip-desktop-native.exe --workspace <FOLDER>
README

        python3 -c "
import zipfile, pathlib, sys
out = pathlib.Path(sys.argv[1])
src = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(out, 'w', zipfile.ZIP_DEFLATED) as zf:
    for f in sorted(src.rglob('*')):
        if f.is_file():
            zf.write(f, f.relative_to(src.parent))
" "$ZIP_OUT" "$PKG_DIR"
        echo "Created package: $ZIP_OUT"

        # Per-user installer. MSYS_NO_PATHCONV keeps Git Bash from rewriting
        # the /D, /O and /F switches; paths are converted explicitly instead.
        MSYS_NO_PATHCONV=1 "$ISCC" /Qp \
            "/DAppVersion=${VERSION}" \
            "/DSourceExe=$(cygpath -w "$PKG_DIR/snip-desktop-native.exe")" \
            "/DIconFile=$(cygpath -w "$ICON_DIR/icon.ico")" \
            "/DLicenseDir=$(cygpath -w "$PKG_DIR/licenses")" \
            "/O$(cygpath -w "$OUT_DIR")" \
            "/Fsnip-sync-windows-setup" \
            "$(cygpath -w "$ROOT_DIR/crates/desktop-native/packaging/windows/snip-sync.iss")"
        test -s "$OUT_DIR/snip-sync-windows-setup.exe"
        echo "Created installer: $OUT_DIR/snip-sync-windows-setup.exe"
        ;;

    x86_64-unknown-linux-gnu)
        TAR_OUT="$OUT_DIR/snip-sync-linux-x86_64.tar.gz"
        PKG_NAME="snip-sync-${VERSION}"
        PKG_DIR="$STAGE_DIR/$PKG_NAME"
        mkdir -p "$PKG_DIR/bin" "$PKG_DIR/share/applications"

        cp "$BIN_PATH" "$PKG_DIR/bin/snip-desktop-native"
        chmod 755 "$PKG_DIR/bin/snip-desktop-native"
        stage_licenses "$PKG_DIR/licenses"

        cat > "$PKG_DIR/share/applications/snip-sync.desktop" <<DESKTOP
[Desktop Entry]
Name=snip-sync
Comment=Native Git Workbench & Snippet Synchronizer
Exec=snip-desktop-native %u
Terminal=false
Type=Application
Categories=Development;RevisionControl;
Version=1.0
DESKTOP

        cat > "$PKG_DIR/README.txt" <<README
snip-sync ${VERSION} (native desktop app)
Version: ${VERSION}
Target: ${TARGET}

Installation:
  Copy bin/snip-desktop-native to your PATH (e.g. ~/.local/bin or /usr/local/bin).
README

        tar -czf "$TAR_OUT" -C "$STAGE_DIR" "$PKG_NAME"
        echo "Created package: $TAR_OUT"
        ;;

    *)
        echo "Error: Unsupported target triple '$TARGET'." >&2
        echo "Supported targets: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc" >&2
        exit 1
        ;;
esac

# Generate target-specific checksum file (avoids collisions during multi-target CI collection)
CHECKSUM_FILE="$OUT_DIR/SHA256SUMS-${TARGET}.txt"
echo "Generating target-specific checksums in $CHECKSUM_FILE..."
python3 -c "
import hashlib, pathlib, sys
out_dir = pathlib.Path(sys.argv[1])
sums_path = pathlib.Path(sys.argv[2])
lines = []
for p in sorted(out_dir.glob('*')):
    if p.is_file() and not p.name.startswith('SHA256SUMS'):
        h = hashlib.sha256(p.read_bytes()).hexdigest()
        lines.append(f'{h}  {p.name}\n')
sums_path.write_text(''.join(lines), encoding='utf-8')
" "$OUT_DIR" "$CHECKSUM_FILE"

echo "Package pipeline complete for $TARGET."
