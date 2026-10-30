#!/bin/sh
# Build TimeTrack as an AppImage.
#
#   ./build-appimage.sh
#
# Produces TimeTrack-<arch>.AppImage: one portable file bundling
# timetrack-gui, timetrack-service, the timetrack CLI and timetrack-tray.
# It needs no install -- its AppRun starts the bundled service in the
# background, then the GUI (or the CLI / tray, when invoked as
# `timetrack` / `timetrack-tray` or with `cli` / `tray`).
#
# Uses linuxdeploy (library bundling) and appimagetool (image assembly),
# fetched on demand from their continuous releases. Both run with
# --appimage-extract-and-run so no FUSE mount is needed to build.
set -eu

cd "$(dirname "$0")"

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|aarch64) ;;
    *) echo "unsupported architecture: $ARCH (need x86_64 or aarch64)" >&2; exit 2 ;;
esac

APPIMAGE="TimeTrack-$ARCH.AppImage"
LINUXDEPLOY="linuxdeploy-$ARCH.AppImage"
APPIMAGETOOL="appimagetool-$ARCH.AppImage"
LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/$LINUXDEPLOY"
# appimagetool is pinned: its `continuous` build changed validation on
# 2026-10-29 and silently broke every release (#135). Bumping the tool is
# a deliberate edit here, verified by a release dry-run.
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/1.9.1/$APPIMAGETOOL"

# Update information, embedded by appimagetool (`-u` below). It tells
# AppImageUpdate where to look: the latest GitHub release's zsync file for
# this architecture, published by `release.yml` next to the AppImage. When
# zsyncmake is installed, appimagetool also writes that `.zsync` file next
# to the image; without it the info is still embedded and only the file is
# missing (the release job installs `zsync`, so a published image always
# has both). The in-app updater needs neither: it reads the same releases
# page directly.
UPDATE_INFO="gh-releases-zsync|sequ-r|timetrack|latest|TimeTrack-*$ARCH.AppImage.zsync"

for pair in "$LINUXDEPLOY $LINUXDEPLOY_URL" "$APPIMAGETOOL $APPIMAGETOOL_URL"; do
    # shellcheck disable=SC2086
    set -- $pair
    if [ ! -x "$1" ]; then
        echo "fetching $1 ..."
        curl -fL -o "$1" "$2"
        chmod +x "$1"
    fi
done

cargo build --release --locked --package timetrack-gui --package timetrack-service --package timetrack-cli --package timetrack-tray

rm -rf AppDir
mkdir -p AppDir/usr/bin \
    AppDir/usr/share/applications \
    AppDir/usr/share/metainfo \
    AppDir/usr/share/icons/hicolor/scalable/apps \
    AppDir/usr/share/icons/hicolor/256x256/apps \
    AppDir/usr/share/icons/hicolor/symbolic/apps

install -m0755 target/release/timetrack-gui AppDir/usr/bin/timetrack-gui
install -m0755 target/release/timetrack-service AppDir/usr/bin/timetrack-service
install -m0755 target/release/timetrack AppDir/usr/bin/timetrack
install -m0755 target/release/timetrack-tray AppDir/usr/bin/timetrack-tray
install -m0644 data/org.sequ.timetrack.desktop AppDir/usr/share/applications/
# The tray's launcher, rewritten for the image: its host form starts a
# host `timetrack-tray` binary, while inside the AppImage the tray is an
# AppRun mode of the image itself (#105).
sed 's/^Exec=timetrack-tray$/Exec=timetrack-gui tray/' \
    data/org.sequ.timetrack.tray.desktop \
    > AppDir/usr/share/applications/org.sequ.timetrack.tray.desktop
chmod 0644 AppDir/usr/share/applications/org.sequ.timetrack.tray.desktop
install -m0644 data/org.sequ.timetrack.metainfo.xml AppDir/usr/share/metainfo/
install -m0644 data/icons/hicolor/scalable/apps/org.sequ.timetrack.svg \
    AppDir/usr/share/icons/hicolor/scalable/apps/
# The PNG is what makes `.DirIcon` a regular file: appimagetool rejects
# an image whose only icon is SVG (#135).
install -m0644 data/icons/hicolor/256x256/apps/org.sequ.timetrack.png \
    AppDir/usr/share/icons/hicolor/256x256/apps/
install -m0644 data/icons/hicolor/symbolic/apps/org.sequ.timetrack-symbolic.svg \
    AppDir/usr/share/icons/hicolor/symbolic/apps/

# Bundle the linked libraries and install the desktop file, icon and
# generated AppRun into AppDir.
"./$LINUXDEPLOY" --appimage-extract-and-run --appdir AppDir \
    -e AppDir/usr/bin/timetrack-gui \
    -e AppDir/usr/bin/timetrack-service \
    -e AppDir/usr/bin/timetrack \
    -e AppDir/usr/bin/timetrack-tray \
    -d AppDir/usr/share/applications/org.sequ.timetrack.desktop \
    -i AppDir/usr/share/icons/hicolor/scalable/apps/org.sequ.timetrack.svg \
    -i AppDir/usr/share/icons/hicolor/256x256/apps/org.sequ.timetrack.png

# linuxdeploy leaves `.DirIcon` as a symlink to the SVG; appimagetool
# wants a regular PNG file there. Replace it outright (#135).
rm -f AppDir/.DirIcon
install -m0644 data/icons/hicolor/256x256/apps/org.sequ.timetrack.png AppDir/.DirIcon

# Wrap the generated AppRun so the bundled service starts with the app, and
# so the image also serves the terminal client. The service owns the store
# and the bus name: if one is already running (a second copy of this
# AppImage), the extra copy fails to own the name and exits, and the client
# talks to whichever holds it. The GUI already retries its connection, and
# the CLI waits for the service, so no wait is needed here, and the service
# intentionally outlives the GUI, as with D-Bus activation.
#
# Uninstall is deleting the AppImage file: nothing is written to the host
# (no desktop copy, no D-Bus activation file, no systemd unit), so there is
# nothing that can dangle afterwards. A `timetrack` symlink pointing at the
# image breaks instead, which the shell reports plainly.
mv AppDir/AppRun AppDir/AppRun.wrapped
cat > AppDir/AppRun <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
start_service() {
    if [ -x "$HERE/usr/bin/timetrack-service" ]; then
        "$HERE/usr/bin/timetrack-service" >/dev/null 2>&1 &
    fi
}
case "$(basename "$0")" in
    timetrack|timetrack-cli)
        start_service
        exec "$HERE/usr/bin/timetrack" "$@"
        ;;
    timetrack-tray)
        start_service
        exec "$HERE/usr/bin/timetrack-tray" "$@"
        ;;
esac
if [ "${1:-}" = "cli" ]; then
    shift
    start_service
    exec "$HERE/usr/bin/timetrack" "$@"
fi
if [ "${1:-}" = "tray" ]; then
    shift
    start_service
    exec "$HERE/usr/bin/timetrack-tray" "$@"
fi
start_service
exec "$HERE/AppRun.wrapped" "$@"
EOF
chmod +x AppDir/AppRun

# Absolute paths: `--appimage-extract-and-run` re-executes the tool from an
# extracted copy that does not keep this script's working directory, so a
# relative `AppDir` fails with "no such file or directory".
"./$APPIMAGETOOL" --appimage-extract-and-run -u "$UPDATE_INFO" "$PWD/AppDir" "$PWD/$APPIMAGE"

# appimagetool writes the `.zsync` next to wherever its re-executed copy
# thinks it runs (in practice `$HOME`), not next to the image. Move it into
# place so the release job finds it. A missing file only warns: without
# zsyncmake nothing is generated, but the update information above is still
# embedded (the release job installs `zsync`, so a published image always
# has both -- and fails the release if it does not).
for candidate in "$HOME/$APPIMAGE.zsync"; do
    if [ -f "$candidate" ]; then
        mv "$candidate" "$PWD/$APPIMAGE.zsync"
    fi
done
if [ -f "$PWD/$APPIMAGE.zsync" ]; then
    echo "wrote $APPIMAGE.zsync"
else
    echo "warning: no .zsync generated (zsyncmake missing?); update info is still embedded" >&2
fi
echo "built $APPIMAGE"
