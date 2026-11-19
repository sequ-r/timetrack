#!/usr/bin/env bash
# Build the TimeTrack macOS bundle and disk image (#159).
#
#   ./packaging/macos/build-bundle.sh --version 0.2.0
#   # -> dist/TimeTrack-0.2.0-macos-arm64.dmg (+ sha256 printed)
#
# Lays the four release binaries out as TimeTrack.app (see README.md next
# to this script for the layout and why the helpers sit beside the GUI
# instead of in a Helpers subdir), generates the .icns from the shipped
# app icon, stamps Info.plist, signs everything with the Hardened Runtime
# when a maintainer certificate is present, and ships the bundle inside a
# drag-to-Applications DMG that is notarized and stapled when the Apple
# API secrets are present.
#
# Options:
#   --version X.Y.Z   release version (default: workspace Cargo.toml,
#                     which release.yml has already minted before calling)
#   --skip-build      skip `cargo build`; package target/release/ as-is
#   --output-dir DIR  where the finished .dmg lands (default: dist/)
#
# Signing needs a maintainer-supplied certificate in the environment (see
# README.md): APPLE_CERT_P12_BASE64 (+ optional APPLE_CERT_PASSWORD) and
# APPLE_DEVELOPER_ID. Notarization additionally needs APPLE_ID,
# APPLE_APP_PASSWORD and APPLE_TEAM_ID. Without the certificate the build
# still succeeds but prints an UNSIGNED warning: the DMG installs and runs,
# it just fails Gatekeeper and needs the right-click-Open escape hatch.
# Releases must say so explicitly in their notes.
#
# macOS only: icon generation (sips/iconutil), signing (codesign) and the
# disk image (hdiutil) are system tools with no Linux equivalents. Each is
# checked at its point of use with a plain error, so a run elsewhere fails
# naming the missing tool instead of halfway through a bundle.
set -euo pipefail

die() { echo "build-bundle: $*" >&2; exit 2; }
need() { command -v "$1" >/dev/null 2>&1 || die "needs $1 ($2)"; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
BUILD_DIR="$SCRIPT_DIR/build"
ENTITLEMENTS="$SCRIPT_DIR/TimeTrack.entitlements"
PLIST_TEMPLATE="$SCRIPT_DIR/Info.plist"
ICON_SRC="$REPO_ROOT/data/icons/hicolor/256x256/apps/org.sequ.timetrack.png"

BINARIES="timetrack-gui timetrack-service timetrack timetrack-tray"

VERSION=""
SKIP_BUILD=0
OUTPUT_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version needs X.Y.Z"
            VERSION="$2"; shift 2;;
        --skip-build)
            SKIP_BUILD=1; shift;;
        --output-dir)
            [ $# -ge 2 ] || die "--output-dir needs a directory"
            OUTPUT_DIR="$2"; shift 2;;
        -h|--help)
            echo "usage: $0 [--version X.Y.Z] [--skip-build] [--output-dir DIR]"
            exit 0;;
        *)
            die "unknown argument: $1";;
    esac
done

if [ -z "$VERSION" ]; then
    VERSION="$(grep -E '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' "$REPO_ROOT/Cargo.toml" \
        | head -1 | sed -E 's/^version = "(.*)"$/\1/')"
    [ -n "$VERSION" ] || die "no workspace version line in Cargo.toml"
fi
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "not a version: $VERSION (need X.Y.Z)"
if [ -z "$OUTPUT_DIR" ]; then
    OUTPUT_DIR="$REPO_ROOT/dist"
fi
mkdir -p "$OUTPUT_DIR"

# Single-arch v1: the runner's own arch goes into the artifact name, so
# the name is truthful on whatever host builds it. Universal2 (lipo) waits
# for a proven Intel half (see README.md).
case "$(uname -m)" in
    arm64|aarch64) ARCH="arm64";;
    x86_64) ARCH="x86_64";;
    *) die "unsupported arch: $(uname -m)";;
esac

echo "TimeTrack macOS bundle, version $VERSION ($ARCH)"

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "-- cargo build --release"
    (cd "$REPO_ROOT" && cargo build --release --locked \
        --package timetrack-gui --package timetrack-service \
        --package timetrack-cli --package timetrack-tray)
fi

# --- bundle layout (README.md, "Bundle layout") ---
APP="$BUILD_DIR/TimeTrack.app"
CONTENTS="$APP/Contents"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
for b in $BINARIES; do
    src="$REPO_ROOT/target/release/$b"
    [ -f "$src" ] || die "missing binary: $src (cargo build --release first)"
    install -m0755 "$src" "$CONTENTS/MacOS/$b"
done
sed "s/__VERSION__/$VERSION/g" "$PLIST_TEMPLATE" > "$CONTENTS/Info.plist"
if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$CONTENTS/Info.plist" >/dev/null \
        || die "stamped Info.plist failed plutil -lint"
    plutil -lint "$ENTITLEMENTS" >/dev/null \
        || die "TimeTrack.entitlements failed plutil -lint"
fi

# --- icon: .icns generated from the shipped 256px PNG ---
# iconutil wants every size spelled out; the 512/1024 entries upscale from
# the 256px master, which is this slice's documented v1 limitation (a
# 1024px master is future work, not a blocker: the icon reads fine, it is
# just not pixel-perfect on Retina).
need sips "macOS only"
need iconutil "macOS only"
[ -f "$ICON_SRC" ] || die "missing icon source: $ICON_SRC"
ICONSET="$BUILD_DIR/TimeTrack.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for spec in "16:icon_16x16.png" "32:icon_16x16@2x.png icon_32x32.png" \
        "64:icon_32x32@2x.png" "128:icon_128x128.png" \
        "256:icon_128x128@2x.png icon_256x256.png" \
        "512:icon_256x256@2x.png icon_512x512.png" \
        "1024:icon_512x512@2x.png"; do
    px="${spec%%:*}"
    names="${spec#*:}"
    for name in $names; do
        sips -z "$px" "$px" "$ICON_SRC" --out "$ICONSET/$name" >/dev/null \
            || die "sips failed on $name"
    done
done
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/TimeTrack.icns" \
    || die "iconutil failed"

# --- signing, when the maintainer certificate is present ---
# The P12 arrives base64'd so it can live in a GitHub secret; the password
# may legitimately be empty. A throwaway keychain holds the import so the
# login keychain is never touched, and the trap below always removes both.
need codesign "macOS only"
xattr -cr "$APP" 2>/dev/null || true
SIGNING=0
KEYCHAIN_DIR=""
KEYCHAIN=""
P12=""
ORIG_KEYCHAINS=()
cleanup() {
    if [ "${#ORIG_KEYCHAINS[@]}" -gt 0 ]; then
        security list-keychains -d user -s "${ORIG_KEYCHAINS[@]}" >/dev/null 2>&1 || true
    fi
    if [ -n "$KEYCHAIN" ] && [ -f "$KEYCHAIN" ]; then
        security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    fi
    if [ -n "$KEYCHAIN_DIR" ] && [ -d "$KEYCHAIN_DIR" ]; then
        rm -rf "$KEYCHAIN_DIR"
    fi
    if [ -n "$P12" ] && [ -f "$P12" ]; then
        rm -f "$P12"
    fi
}
trap cleanup EXIT
if [ -n "${APPLE_CERT_P12_BASE64:-}" ]; then
    need security "macOS only"
    need uuidgen "macOS only"
    [ -n "${APPLE_DEVELOPER_ID:-}" ] \
        || die "APPLE_CERT_P12_BASE64 is set but APPLE_DEVELOPER_ID is empty"
    P12="$(mktemp -t timetrack-sign).p12"
    printf '%s' "$APPLE_CERT_P12_BASE64" | base64 --decode > "$P12"
    KEYCHAIN_PASS="$(uuidgen)"
    KEYCHAIN_DIR="$(mktemp -d -t timetrack-sign)"
    KEYCHAIN="$KEYCHAIN_DIR/build.keychain-db"
    while IFS= read -r kc; do
        kc="${kc#"${kc%%[![:space:]]*}"}"
        kc="${kc%"${kc##*[![:space:]]}"}"
        kc="${kc#\"}"
        kc="${kc%\"}"
        [ -n "$kc" ] && ORIG_KEYCHAINS+=("$kc")
    done < <(security list-keychains -d user)
    security create-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
    security set-keychain-settings -lut 21600 "$KEYCHAIN"
    security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
    security import "$P12" -k "$KEYCHAIN" \
        -P "${APPLE_CERT_PASSWORD:-}" -T /usr/bin/codesign
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
        -k "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null
    security list-keychains -d user -s "$KEYCHAIN" "${ORIG_KEYCHAINS[@]+"${ORIG_KEYCHAINS[@]}"}"
    echo "-- signing bundle (Hardened Runtime)"
    for b in $BINARIES; do
        codesign --force --sign "$APPLE_DEVELOPER_ID" --keychain "$KEYCHAIN" --timestamp \
            --options runtime --entitlements "$ENTITLEMENTS" \
            "$CONTENTS/MacOS/$b" \
            || die "codesign failed on $b"
    done
    codesign --force --sign "$APPLE_DEVELOPER_ID" --keychain "$KEYCHAIN" --timestamp \
        --options runtime --entitlements "$ENTITLEMENTS" "$APP" \
        || die "codesign failed on TimeTrack.app"
    codesign --verify --deep --strict "$APP" \
        || die "signed bundle failed codesign --verify"
    SIGNING=1
else
    echo "build-bundle: UNSIGNED build: no APPLE_CERT_P12_BASE64 in the environment." >&2
    echo "build-bundle: The DMG installs and runs, but Gatekeeper refuses it until" >&2
    echo "build-bundle: the user right-click-Opens it once. Releases must say so" >&2
    echo "build-bundle: explicitly in their notes." >&2
    echo "-- ad-hoc signing bundle (still unsigned for Gatekeeper)"
    for b in $BINARIES; do
        codesign --force --sign - "$CONTENTS/MacOS/$b" \
            || die "ad-hoc codesign failed on $b"
    done
    codesign --force --sign - "$APP" \
        || die "ad-hoc codesign failed on TimeTrack.app"
    codesign --verify --deep --strict "$APP" \
        || die "ad-hoc signed bundle failed codesign --verify"
fi

# --- disk image: the bundle plus a drop target ---
need hdiutil "macOS only"
DMG_ROOT="$BUILD_DIR/dmgroot"
DMG="$OUTPUT_DIR/TimeTrack-$VERSION-macos-$ARCH.dmg"
rm -rf "$DMG_ROOT" "$DMG"
mkdir -p "$DMG_ROOT"
cp -R "$APP" "$DMG_ROOT/TimeTrack.app"
ln -s /Applications "$DMG_ROOT/Applications"
hdiutil create -volname "TimeTrack" -srcfolder "$DMG_ROOT" \
    -ov -format UDZO "$DMG" >/dev/null \
    || die "hdiutil create failed"
if [ "$SIGNING" -eq 1 ]; then
    codesign --force --sign "$APPLE_DEVELOPER_ID" --keychain "$KEYCHAIN" --timestamp "$DMG" \
        || die "codesign failed on the DMG"
fi

# --- notarization, when the API secrets join the certificate ---
# An unsigned artifact cannot be notarized, so this step needs both halves;
# either half missing degrades to the documented unsigned story, never to
# a half-notarized artifact that looks official but is not.
if [ "$SIGNING" -eq 1 ] && [ -n "${APPLE_ID:-}" ] \
        && [ -n "${APPLE_APP_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    need xcrun "macOS only (Xcode command line tools)"
    echo "-- notarizing (notarytool) and stapling"
    xcrun notarytool submit "$DMG" --wait \
        --apple-id "$APPLE_ID" --password "$APPLE_APP_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" \
        || die "notarization failed"
    xcrun stapler staple "$DMG" || die "stapling failed"
    xcrun stapler validate "$DMG" || die "staple validation failed"
elif [ "$SIGNING" -eq 1 ]; then
    echo "build-bundle: signed but NOT NOTARIZED: set APPLE_ID," >&2
    echo "build-bundle: APPLE_APP_PASSWORD and APPLE_TEAM_ID to notarize." >&2
    echo "build-bundle: Gatekeeper still warns on first launch of an" >&2
    echo "build-bundle: unnotarized signed build; releases must say so." >&2
fi

echo "dmg:    $DMG"
shasum -a 256 "$DMG" | sed 's| .*|  (above)|'
