#!/usr/bin/env bash
# Headless smoke check for the GUI.
#
# The unit tests cover the view's rules but never open a window, so nothing in
# `cargo test` can say whether the toolkit still paints, hit-tests or reads the
# keyboard. This script covers that gap: it starts a private bus, a real
# service and the real GUI on Xvfb, then
#
#   - screenshots the window and asserts the app painted (theme background,
#     the week total's glyphs, and four quick-add buttons),
#   - clicks the 15-minute button through the X11 XTest extension and asserts
#     the click reached the service as a quick-add,
#   - asserts the week total on screen re-rendered after that click,
#   - presses `1` and then `q`, asserting quick-add by keyboard and that `q`
#     quits.
#   - opens the manual-entry dialog with `a` and asserts it painted,
#     cancels it with Escape, then reopens it, types a description across
#     the three fields and saves, asserting the entry reached the service
#     with the typed text -- and that the window refreshed with no pointer
#     motion at all, proving the background poll repaints on its own.
#
# It reads pixels rather than window properties on purpose: a screenshot is the
# only evidence that the renderer drew something rather than the window merely
# existing. There is no OCR -- the pixel assertions deliberately do not claim
# to read the number, only that the region changed when the total did.
#
# Run it after `cargo build --release`.
#
# Needs: Xvfb, dbus-daemon, ImageMagick (magick/import), xdpyinfo, setxkbmap,
# gcc and libXtst. Xvfb implements no DRI3, and Mesa's GPU drivers require it
# to present, so the app needs a software Vulkan driver:
#
#   - a distro package (Arch: vulkan-swrast, Debian/Ubuntu: mesa-vulkan-drivers)
#     provides one, or
#   - point TT_LAVAPIPE_DIR at an *extracted* vulkan-swrast package, e.g.
#       curl -O https://geo.mirror.pkgbuild.com/extra/os/x86_64/vulkan-swrast-<ver>-x86_64.pkg.tar.zst
#       bsdtar -xf vulkan-swrast-*.pkg.tar.zst -C /tmp/lvp usr/lib/libvulkan_lvp.so usr/share/vulkan/icd.d/lvp_icd.json
#       TT_LAVAPIPE_DIR=/tmp/lvp scripts/gui-smoke.sh
#
# Without one the window opens and stays black, which this script reports as a
# driver problem rather than as a failure of the app.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
GUI=$ROOT/target/release/timetrack-gui
SVC=$ROOT/target/release/timetrack-service
CLI=$ROOT/target/release/timetrack
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tt-gui.XXXXXX")
STORE=$WORK/store.json
SCREEN=1024x900

pass=0; fail=0; skipped=0
ok()    { pass=$((pass+1));       printf '  ok    %s\n' "$1"; }
bad()   { fail=$((fail+1));       printf '  FAIL  %s\n' "$1"; [ $# -gt 1 ] && printf '        %s\n' "$2"; return 0; }
skip()  { skipped=$((skipped+1)); printf '  skip  %s\n' "$1"; }
check() { # check <desc> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected [$2] got [$3]"; fi
}

cleanup() {
  [ -n "${GUI_PID:-}" ] && kill "$GUI_PID" 2>/dev/null
  [ -n "${SVC_PID:-}" ] && kill "$SVC_PID" 2>/dev/null
  [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null
  wait 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

for f in "$GUI" "$SVC" "$CLI"; do
  [ -x "$f" ] || { echo "missing $f -- run 'cargo build --release' first"; exit 2; }
done
for t in Xvfb dbus-daemon magick import xdpyinfo setxkbmap gcc; do
  command -v "$t" >/dev/null || { echo "missing $t"; exit 2; }
done

# A software Vulkan driver, if the caller pointed us at one.
SOFTWARE_VULKAN=""
if [ -n "${TT_LAVAPIPE_DIR:-}" ]; then
  [ -f "$TT_LAVAPIPE_DIR/usr/lib/libvulkan_lvp.so" ] ||
    { echo "TT_LAVAPIPE_DIR=$TT_LAVAPIPE_DIR has no usr/lib/libvulkan_lvp.so"; exit 2; }
  SOFTWARE_VULKAN=$TT_LAVAPIPE_DIR/usr
fi

echo "== a private bus, a real service, and a headless X server =="
BUS_CONF=$WORK/bus.conf
cat >"$BUS_CONF" <<'XML'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
XML

# No service directories, so the bus cannot auto-activate a stale host service
# and answer as though it were the binary under test.
DBUS_SESSION_BUS_ADDRESS=$(dbus-daemon --config-file="$BUS_CONF" --print-address --fork) ||
  { echo "could not start a private bus"; exit 2; }
export DBUS_SESSION_BUS_ADDRESS

TZ=UTC "$SVC" "$STORE" >"$WORK/svc.log" 2>&1 &
SVC_PID=$!
for _ in $(seq 1 60); do "$CLI" status >/dev/null 2>&1 && break; sleep 0.1; done
"$CLI" status >/dev/null 2>&1 || { echo "service never became ready:"; cat "$WORK/svc.log"; exit 2; }
echo "  service up"

# A display number that is free, so this can run next to a real session.
DISP=""
for n in $(seq 90 99); do
  [ -e "/tmp/.X11-unix/X$n" ] && continue
  DISP=":$n"; break
done
[ -n "$DISP" ] || { echo "no free X display in :90-:99"; exit 2; }
Xvfb "$DISP" -screen 0 "${SCREEN}x24" -nolisten tcp >"$WORK/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 60); do xdpyinfo -display "$DISP" >/dev/null 2>&1 && break; sleep 0.1; done
xdpyinfo -display "$DISP" >/dev/null 2>&1 || { echo "Xvfb never came up"; cat "$WORK/xvfb.log"; exit 2; }
echo "  xvfb up on $DISP"

# Xvfb has no keymap of its own, and gpui maps keysyms through it: without one
# every keystroke is unrepresentable. Loading a keymap is what makes the
# keyboard checks below possible at all.
KEYMAP=no
setxkbmap -display "$DISP" us >/dev/null 2>&1 && KEYMAP=yes
echo "  keymap: $KEYMAP"

# The XTest helper: injecting input and reading pixels is the test's job, not
# the app's, so this is a throwaway C program rather than a dependency.
cat >"$WORK/xtest.c" <<'C'
// Minimal XTest driver: `xtest <display> move|click <x> <y>` or
// `xtest <display> key <keysym>`. Exits 3 for a keysym the server has no
// keycode for, which is how a keymap-less X server reports itself.
#include <X11/Xlib.h>
#include <X11/extensions/XTest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 4) { return 2; }
    Display *d = XOpenDisplay(argv[1]);
    if (!d) { fprintf(stderr, "cannot open %s\n", argv[1]); return 2; }

    if (strcmp(argv[2], "move") == 0) {
        XTestFakeMotionEvent(d, -1, atoi(argv[3]), atoi(argv[4]), CurrentTime);
        XFlush(d); usleep(200000);
    } else if (strcmp(argv[2], "click") == 0) {
        // Move first: no window system delivers a click without a preceding
        // motion, and gpui hit-tests from the hover.
        XTestFakeMotionEvent(d, -1, atoi(argv[3]), atoi(argv[4]), CurrentTime);
        XFlush(d); usleep(200000);
        XTestFakeButtonEvent(d, 1, True, CurrentTime);
        XFlush(d); usleep(50000);
        XTestFakeButtonEvent(d, 1, False, CurrentTime);
        XFlush(d); usleep(200000);
    } else if (strcmp(argv[2], "key") == 0) {
        KeyCode kc = XKeysymToKeycode(d, XStringToKeysym(argv[3]));
        if (kc == 0) { fprintf(stderr, "no keycode for %s\n", argv[3]); XCloseDisplay(d); return 3; }
        XTestFakeKeyEvent(d, kc, True, CurrentTime);
        XFlush(d); usleep(50000);
        XTestFakeKeyEvent(d, kc, False, CurrentTime);
        XFlush(d); usleep(200000);
    } else {
        XCloseDisplay(d); return 2;
    }
    XCloseDisplay(d);
    return 0;
}
C
gcc -O1 -o "$WORK/xtest" "$WORK/xtest.c" -lX11 -lXtst || { echo "could not build the XTest helper"; exit 2; }

shot() { DISPLAY=$DISP import -window root "$WORK/$1" 2>/dev/null; }

# Connected components of one colour, as "w h x y area". Fuzz 5% absorbs the
# rounding the GPU path may do, and is below the distance between the colours
# used here -- #11111b against black, which is what the window would be if
# nothing had been drawn. `+fuzz` resets it so the second pass is exact.
blobs() { # blobs <png> <#rrggbb>
  magick "$WORK/$1" -fuzz 5% -fill white -opaque "$2" +fuzz -fill black +opaque white "$WORK/mask.png" 2>/dev/null
  magick "$WORK/mask.png" -define connected-components:verbose=true -connected-components 8 null: 2>&1 |
    sed -nE 's/^ *[0-9]+: ([0-9]+)x([0-9]+)\+([0-9]+)\+([0-9]+) [^ ]+ ([0-9]+) gray\(255\)$/\1 \2 \3 \4 \5/p'
}

echo
echo "== the window paints =="
(
  export DISPLAY=$DISP WAYLAND_DISPLAY= DBUS_SESSION_BUS_ADDRESS TZ=UTC
  # Empty WAYLAND_DISPLAY is deliberate: gpui picks Wayland when it is set and
  # non-empty, and there is no Wayland compositor on this display.
  [ -n "$SOFTWARE_VULKAN" ] &&
    export VK_DRIVER_FILES="$SOFTWARE_VULKAN/share/vulkan/icd.d/lvp_icd.json" LD_LIBRARY_PATH="$SOFTWARE_VULKAN/lib"
  exec "$GUI"
) >"$WORK/gui.log" 2>&1 &
GUI_PID=$!

sleep 1
# Xvfb runs no window manager, so no ConfigureNotify ever reaches the window
# and gpui has nothing to draw its first frame for: the window is mapped, the
# right size, and stays black until an event arrives. A pointer motion into it
# is such an event -- and the pointer has to end up over the window for the
# keyboard below, since without a window manager the input focus is
# PointerRoot.
"$WORK/xtest" "$DISP" move $(( ${SCREEN%x*} / 2 )) $(( ${SCREEN#*x} / 2 ))

WINDOW=""
for _ in $(seq 1 100); do
  if ! kill -0 "$GUI_PID" 2>/dev/null; then
    echo "the GUI exited before it painted:"; cat "$WORK/gui.log"; exit 1
  fi
  shot boot.png
  WINDOW=$(blobs boot.png '#11111b' | awk '$1 > 600 && $2 > 600 {print; exit}')
  [ -n "$WINDOW" ] && break
  sleep 0.2
done
if [ -n "$WINDOW" ]; then
  ok "the app painted a window ${WINDOW%% *}x$(echo "$WINDOW" | awk '{print $2}')"
else
  if grep -qi "DRI3" "$WORK/gui.log"; then
    bad "the window painted" "the GPU driver cannot present on Xvfb (no DRI3) -- see the header for a software Vulkan driver"
  else
    bad "the window painted" "nothing of colour #11111b appeared in 20s"
  fi
  echo "gui log:"; cat "$WORK/gui.log"; exit 1
fi

# The window's own rectangle, from that blob, so the crops below follow the
# window wherever the app put it.
WINDOW_X=$(echo "$WINDOW" | awk '{print $3}')
WINDOW_Y=$(echo "$WINDOW" | awk '{print $4}')
# The band holding the week total: under the tab strip, above quick-add.
HERO="$(echo "$WINDOW" | awk '{print $1}')x130+${WINDOW_X}+$(( WINDOW_Y + 50 ))"

# The quick-add row is the only place the button green appears in four blocks
# of similar size. The `+` that marks a quick-added entry is the same colour
# but a few pixels wide, and the "Start the service" button is much wider.
buttons() { blobs "$1" '#2ea043' | awk '$1 >= 30 && $1 <= 80 && $2 >= 18 && $2 <= 40 {print $3" "$4" "$1" "$2}' | sort -n; }

for _ in $(seq 1 30); do
  shot ready.png
  [ "$(buttons ready.png | wc -l)" = "4" ] && break
  sleep 0.3
done
check "four quick-add buttons are drawn" "4" "$(buttons ready.png | wc -l)"

# The service is running, so the window must not still be offering to start it:
# that button is the same green, and seeing it means the GUI never connected.
if blobs ready.png '#2ea043' | awk '$1 > 100 {found=1} END {exit !found}'; then
  bad "the GUI connected to the service" "it is still showing 'Start the service'"
else
  ok "the GUI connected to the service"
fi

# Text, not just boxes. The week total is white and it is the largest white
# thing on screen; this is also the font-system check -- no fonts, no glyphs.
magick "$WORK/ready.png" -crop "$HERO" +repage "$WORK/hero.png" 2>/dev/null
WHITE=$(magick "$WORK/hero.png" -fuzz 5% -fill white -opaque white +fuzz -fill black +opaque white \
  -format '%[fx:mean*w*h]' info: 2>/dev/null)
if [ "${WHITE%%.*}" -gt 200 ] 2>/dev/null; then
  ok "the week total is drawn as text (${WHITE%%.*} white pixels)"
else
  bad "the week total is drawn as text" "only [$WHITE] white pixels in the total's band"
fi

echo
echo "== a click on a quick-add button reaches the service =="
check "the fresh store is empty" "00:00:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"

# The second of the four buttons, left to right, is the 15-minute one.
CLICK=$(buttons ready.png | sed -n '2p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
echo "  clicking ($CLICK)"
"$WORK/xtest" "$DISP" click ${CLICK}
sleep 2.5   # the view polls the service once a second

check "the click added 15 minutes" "00:15:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
check "it was recorded as a quick add" "1" "$("$CLI" list | grep -c '^+')"

# Move the pointer off the button first, so a hover change cannot be mistaken
# for the number having been redrawn, then compare the total's own band.
"$WORK/xtest" "$DISP" move 5 5
sleep 0.5
shot after.png
magick "$WORK/after.png" -crop "$HERO" +repage "$WORK/hero-after.png" 2>/dev/null
DIFF=$(magick compare -metric AE "$WORK/hero.png" "$WORK/hero-after.png" null: 2>&1 | sed 's/ .*//')
if [ "${DIFF:-0}" != "0" ]; then
  ok "the week total on screen re-rendered ($DIFF pixels changed)"
else
  bad "the week total on screen re-rendered" "the band is pixel-identical before and after"
fi

echo
echo "== the keyboard =="
# The key events below only reach the app if the pointer is over it: with no
# window manager the input focus is PointerRoot, so the window under the
# pointer is the one that gets the keystroke. The comparison above deliberately
# moved the pointer away, so bring it back.
"$WORK/xtest" "$DISP" move $(( WINDOW_X + 20 )) $(( WINDOW_Y + 20 ))
if [ "$KEYMAP" != yes ]; then
  skip "quick-add by keyboard and q to quit: this X server has no keymap"
elif "$WORK/xtest" "$DISP" key 1; then
  sleep 2.5
  check "pressing 1 adds 5 minutes" "00:20:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"

  echo
  echo "== the manual-entry dialog =="
  # `a` opens the dialog; Escape closes it without adding anything.
  "$WORK/xtest" "$DISP" key a
  sleep 1.5
  shot dialog.png
  # The dialog panel sits above the week total and pushes it down, so the
  # total's own band must differ while the dialog is open: pixels proving the
  # dialog rendered rather than the key merely doing nothing.
  magick "$WORK/dialog.png" -crop "$HERO" +repage "$WORK/hero-dialog.png" 2>/dev/null
  DIFF=$(magick compare -metric AE "$WORK/hero-after.png" "$WORK/hero-dialog.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the entry dialog painted ($DIFF pixels moved)"
  else
    bad "the entry dialog painted" "the total's band is unchanged with the dialog open"
  fi
  "$WORK/xtest" "$DISP" key Escape
  sleep 1
  check "escape closes the dialog with nothing added" "00:20:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  # `a` again, then type across the fields and save. Focus starts on the
  # description; start/end are prefilled (-60/now), so two Tabs and Return
  # record an hour ending now.
  "$WORK/xtest" "$DISP" key a
  sleep 1
  for k in s m o k e; do "$WORK/xtest" "$DISP" key "$k"; done
  "$WORK/xtest" "$DISP" key Tab
  "$WORK/xtest" "$DISP" key Tab
  "$WORK/xtest" "$DISP" key Return
  sleep 2.5
  check "the dialog added an hour" "01:20:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  check "it carries the typed description" "1" "$("$CLI" list | grep -c smoke)"
  # No pointer motion since the keyboard section began, so a repainted total
  # proves the background poll refreshes the window on its own rather than
  # waiting for the next hover to happen along.
  shot saved.png
  magick "$WORK/saved.png" -crop "$HERO" +repage "$WORK/hero-saved.png" 2>/dev/null
  DIFF=$(magick compare -metric AE "$WORK/hero-after.png" "$WORK/hero-saved.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the window refreshed without any pointer motion ($DIFF pixels changed)"
  else
    bad "the window refreshed without any pointer motion" "the total's band is unchanged after the save"
  fi

  "$WORK/xtest" "$DISP" key q
  QUIT=no
  for _ in $(seq 1 30); do kill -0 "$GUI_PID" 2>/dev/null || { QUIT=yes; break; }; sleep 0.1; done
  check "pressing q quits" "yes" "$QUIT"
  [ "$QUIT" = yes ] && GUI_PID=""
else
  skip "quick-add by keyboard and q to quit: no keycode for '1' on this X server"
fi

echo
echo "== $pass passed, $fail failed, $skipped skipped =="
[ "$fail" = 0 ]
