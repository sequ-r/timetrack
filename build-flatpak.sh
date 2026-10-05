#!/bin/sh
# Build and install TimeTrack.
#
#   ./build-flatpak.sh            # GNOME 50 (stable), the default
#   ./build-flatpak.sh beta       # GNOME 51 (beta)
#
# The cache clear is not optional. flatpak-builder mirrors the git source into
# .flatpak-builder/git/ and reuses it, so after `git pull` the mirror can still
# sit on an older commit than origin/main -- the build then fails on a file
# that plainly exists in the checkout. That exact confusion cost a build once.
set -eu

MANIFEST=org.sequ.timetrack.json
case "${1:-}" in
    beta) MANIFEST=org.sequ.timetrack.beta.json ;;
    "") ;;
    *) echo "usage: $0 [beta]" >&2; exit 2 ;;
esac

cd "$(dirname "$0")"

rm -rf .flatpak-builder build

# NOTE: --disable-rofiles-fuse is required where FUSE mounts are not
# permitted (containers, restricted sandboxes). Without it the build fails
# with "fusermount3: mount failed: Operation not permitted" when spawning
# rofiles-fuse. Unpacking instead of FUSE-mounting is slower but equivalent.
exec flatpak-builder \
    --disable-updates \
    --disable-rofiles-fuse \
    --force-clean \
    --user \
    --install \
    build "$MANIFEST"
