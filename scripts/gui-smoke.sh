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
#   - splits an entry with `s` by typing a moment, and merges the halves
#     back with `m` `m` through the confirmation, asserting counts and the
#     unchanged total through the CLI after each step.
#   - names a project with `n` and renames one with `r` through the naming
#     prompt, proving each typed name reached the store.
#   - undoes the newest quick-add with `u`, asserting the total dropped by
#     exactly that entry through the CLI.
#   - deletes by keyboard with `d`: the first press only paints the
#     confirmation, Escape cancels it with nothing deleted, and `d` `d`
#     removes the entry and moves the total.
#   - switches to the Export tab with `Tab` `Tab`, presses `[` and `]`
#     asserting the anchor stepper repainted and returned, steps to an
#     empty week asserting the scoped preview emptied, then clicks
#     "Export week" and asserts each period-stamped file landed with the
#     right rows plus a footer status, and clicks "Export all" and asserts
#     the CSV file landed with the spec header. A portal stub records each
#     save dialog's prefilled name, asserting the dialog wiring without a
#     live portal (the files still land via the home-directory fallback).
#   - cycles the three tabs with `Tab`, asserting the window changed and
#     that three presses return it pixel-identical to Home.
#
# It reads pixels rather than window properties on purpose: a screenshot is the
# only evidence that the renderer drew something rather than the window merely
# existing. There is no OCR -- the pixel assertions deliberately do not claim
# to read the number, only that the region changed when the total did.
#
# Run it after
#   cargo build --release --features timetrack-gui/portal-stub
# The feature compiles in the save-dialog stub this check drives: there is no
# live portal on a headless X server, and a released binary has no stub to
# arm at all.
#
# Needs: Xvfb, dbus-daemon, ImageMagick (convert, compare, import), xdpyinfo,
# setxkbmap, gcc and libXtst. xwininfo is optional: when it is installed, a
# failed run also prints the X window tree. This spells the ImageMagick
# commands the way both major versions ship them: 6 splits them into separate
# binaries, 7 still installs those alongside its `magick` multiplexer, so one
# spelling runs on both. Xvfb implements no DRI3, and Mesa's GPU drivers
# require it to present, so the app needs a software Vulkan driver:
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
contains(){ # contains <desc> <needle> <haystack>
  case "$3" in *"$2"*) ok "$1";; *) bad "$1" "expected [$2] in [$(printf '%s' "$3" | head -3)]";; esac
}

cleanup() {
  [ -n "${GUI_PID:-}" ] && kill "$GUI_PID" 2>/dev/null
  [ -n "${SVC_PID:-}" ] && kill "$SVC_PID" 2>/dev/null
  [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null
  wait 2>/dev/null
  # A failing run is the one worth keeping: the screenshots and the logs are the
  # evidence, and a CI runner throws its work directory away. Point
  # TT_SMOKE_ARTIFACTS at a directory to have them copied there first.
  if [ "$fail" -gt 0 ] && [ -n "${TT_SMOKE_ARTIFACTS:-}" ]; then
    mkdir -p "$TT_SMOKE_ARTIFACTS"
    cp "$WORK"/*.png "$WORK"/*.log "$TT_SMOKE_ARTIFACTS"/ 2>/dev/null
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

for f in "$GUI" "$SVC" "$CLI"; do
  [ -x "$f" ] || {
    echo "missing $f -- run 'cargo build --release --features timetrack-gui/portal-stub' first"
    exit 2
  }
done
for t in Xvfb dbus-daemon convert compare import xdpyinfo setxkbmap gcc; do
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
# §16 hermeticity (mirrors e2e.sh): clients probe the service socket before
# the bus, so point it into the rig -- the rig service binds it, and the
# GUI/CLI can never reach (or be refused by) a real running service.
export TIMETRACK_SOCKET="$WORK/tt.sock"

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
  convert "$WORK/$1" -fuzz 5% -fill white -opaque "$2" +fuzz -fill black +opaque white "$WORK/mask.png" 2>/dev/null
  convert "$WORK/mask.png" -define connected-components:verbose=true -connected-components 8 null: 2>&1 |
    sed -nE 's/^ *[0-9]+: ([0-9]+)x([0-9]+)\+([0-9]+)\+([0-9]+) [^ ]+ ([0-9]+) gray\(255\)$/\1 \2 \3 \4 \5/p'
}

echo
echo "== the window paints =="
(
  export DISPLAY=$DISP WAYLAND_DISPLAY= DBUS_SESSION_BUS_ADDRESS TZ=UTC HOME=$WORK
  # Portal stub (ticket 27): no live portal exists headless, so the dialog
  # records its prefilled name here and the home-directory fallback runs on.
  export TIMETRACK_PORTAL_STUB="record:$WORK/portal-names.log"
  # No self-update check headless: the footer hints and any "update
  # available" status would depend on the network and on whatever release
  # happens to be latest, and pixel assertions must not.
  export TIMETRACK_NO_UPDATE=1
  # Empty WAYLAND_DISPLAY is deliberate: gpui picks Wayland when it is set and
  # non-empty (see the `wayland` skill, `references/linux-session.md`), and
  # there is no Wayland compositor on this Xvfb display. WAYLAND_DEBUG passes
  # through when the caller sets it, so a protocol trace can be taken without
  # editing the script.
  # HOME points at the scratch dir so the Export buttons below write their
  # files where this script can assert on them, not into the real home.
  [ -n "$SOFTWARE_VULKAN" ] &&
    export VK_DRIVER_FILES="$SOFTWARE_VULKAN/share/vulkan/icd.d/lvp_icd.json" LD_LIBRARY_PATH="$SOFTWARE_VULKAN/lib"
  exec "$GUI"
) >"$WORK/gui.log" 2>&1 &
GUI_PID=$!

# Xvfb runs no window manager, so no ConfigureNotify ever reaches the window
# and gpui has nothing to draw its first frame for: the window is mapped, the
# right size, and stays black until an event arrives. A pointer motion into it
# is such an event -- and the pointer has to end up over the window for the
# keyboard below, since without a window manager the input focus is
# PointerRoot.
#
# The nudge is repeated until something is on screen: a motion sent before the
# window exists is delivered to nothing, so sending one after a fixed `sleep 1`
# races window creation -- comfortably won on a warm desktop, lost on a cold CI
# runner.
NUDGE_X=$(( ${SCREEN%x*} / 2 )); NUDGE_Y=$(( ${SCREEN#*x} / 2 ))
WINDOW=""
for _ in $(seq 1 100); do
  if ! kill -0 "$GUI_PID" 2>/dev/null; then
    echo "the GUI exited before it painted:"; cat "$WORK/gui.log"; exit 1
  fi
  "$WORK/xtest" "$DISP" move "$NUDGE_X" "$NUDGE_Y"
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
  echo "gui log:"; cat "$WORK/gui.log"
  # Whether a window exists at all separates "rendered nothing" from "never
  # created one"; the Vulkan drivers answer the first case, and the background
  # rows say whether the pixel readback itself worked. Only the window tree
  # needs a tool of its own, so it is the one piece that gives way when absent.
  if command -v xwininfo >/dev/null; then
    echo "x window tree:"; xwininfo -display "$DISP" -root -tree 2>&1
  else
    echo "x window tree: (no xwininfo installed)"
  fi
  echo "vulkan drivers:"; ls -1 /usr/share/vulkan/icd.d/ 2>&1
  echo "imagemagick: $(convert -version 2>/dev/null | sed -n 1p)"
  echo "theme-colour rows:"; blobs boot.png '#11111b' | sed -n '1,4p'
  exit 1
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
convert "$WORK/ready.png" -crop "$HERO" +repage "$WORK/hero.png" 2>/dev/null
WHITE=$(convert "$WORK/hero.png" -fuzz 5% -fill white -opaque white +fuzz -fill black +opaque white \
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
convert "$WORK/after.png" -crop "$HERO" +repage "$WORK/hero-after.png" 2>/dev/null
DIFF=$(compare -metric AE "$WORK/hero.png" "$WORK/hero-after.png" null: 2>&1 | sed 's/ .*//')
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
  convert "$WORK/dialog.png" -crop "$HERO" +repage "$WORK/hero-dialog.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-after.png" "$WORK/hero-dialog.png" null: 2>&1 | sed 's/ .*//')
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
  convert "$WORK/saved.png" -crop "$HERO" +repage "$WORK/hero-saved.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-after.png" "$WORK/hero-saved.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the window refreshed without any pointer motion ($DIFF pixels changed)"
  else
    bad "the window refreshed without any pointer motion" "the total's band is unchanged after the save"
  fi

  echo
  echo "== split and merge by keyboard =="
  # A disjoint hour for the GUI to cut: far from the dialog hour (-60..0)
  # and the quick-adds (~now), so the merge below finds exactly its halves.
  "$CLI" add -S -240 -E -180 -d "gui split" >/dev/null
  sleep 2.5  # let the GUI's snapshot pick the entry up (it polls each second)
  # The new entry is the oldest of four, so move down three rows to it.
  "$WORK/xtest" "$DISP" key j
  "$WORK/xtest" "$DISP" key j
  "$WORK/xtest" "$DISP" key j
  "$WORK/xtest" "$DISP" key s
  sleep 1.5
  shot split.png
  # The prompt sits above the week total and pushes it down, like the
  # dialog: the total's band must differ while it is open.
  convert "$WORK/split.png" -crop "$HERO" +repage "$WORK/hero-split.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-after.png" "$WORK/hero-split.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the split prompt painted ($DIFF pixels moved)"
  else
    bad "the split prompt painted" "the total's band is unchanged with the prompt open"
  fi
  # -200 is safely interior to -240..-180 no matter the seconds of drift.
  for k in minus 2 0 0; do "$WORK/xtest" "$DISP" key "$k"; done
  "$WORK/xtest" "$DISP" key Return
  sleep 2.5
  check "split added exactly one entry" "5" "$("$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]')"
  check "split kept the total" "02:20:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  # The second half is now selected; `m` offers its touching first half and
  # states the (zero) delta, and `m` again executes it.
  "$WORK/xtest" "$DISP" key m
  sleep 1.5
  shot merge.png
  convert "$WORK/merge.png" -crop "$HERO" +repage "$WORK/hero-merge.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-after.png" "$WORK/hero-merge.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the merge confirmation painted ($DIFF pixels moved)"
  else
    bad "the merge confirmation painted" "the total's band is unchanged with the confirmation open"
  fi
  "$WORK/xtest" "$DISP" key m
  sleep 2.5
  check "merge fused the halves back into one entry" "4" "$("$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]')"
  check "merge kept the total" "02:20:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"

  echo
  echo "== project naming by keyboard =="
  # `n` opens the naming prompt (no more invented "Project N"); the panel
  # sits above the week total and pushes it down, like the other prompts.
  "$WORK/xtest" "$DISP" key n
  sleep 1.5
  shot project.png
  convert "$WORK/project.png" -crop "$HERO" +repage "$WORK/hero-project.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-after.png" "$WORK/hero-project.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the project prompt painted ($DIFF pixels moved)"
  else
    bad "the project prompt painted" "the total's band is unchanged with the prompt open"
  fi
  # "wonton" is all unbound letters, so a closed prompt would swallow every
  # keystroke harmlessly -- except none of them open anything either, and the
  # duplicate check below would fail loudly.
  for k in w o n t o n; do "$WORK/xtest" "$DISP" key "$k"; done
  "$WORK/xtest" "$DISP" key Return
  sleep 2.5
  out=$("$CLI" project "wonton" 2>&1); contains "the prompt created the typed project" "already exists" "$out"
  # `r` renames the current project (General, cursor untouched throughout):
  # the prompt prefills it, so typing appends.
  "$WORK/xtest" "$DISP" key r
  sleep 1.5
  for k in i s t; do "$WORK/xtest" "$DISP" key "$k"; done
  "$WORK/xtest" "$DISP" key Return
  sleep 2.5
  out=$("$CLI" project "Generalist" 2>&1); contains "the prompt renamed to the typed text" "already exists" "$out"

  echo
  echo "== undo by keyboard =="
  # `u` removes only the newest quick-add (the 5 minutes from the `1`
  # keypress); the hand-entered hours stay.
  "$WORK/xtest" "$DISP" key u
  sleep 2.5
  check "undo removed the 5-minute quick-add" "02:15:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  check "undo left three entries" "3" "$("$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]')"

  echo
  echo "== delete asks first, by keyboard =="
  # To the top row (saturating) and one down: the hour-long "smoke" entry,
  # so the deletion moves the total by an unmistakable hour.
  for _ in $(seq 1 5); do "$WORK/xtest" "$DISP" key k; done
  "$WORK/xtest" "$DISP" key j
  shot delete-base.png
  convert "$WORK/delete-base.png" -crop "$HERO" +repage "$WORK/hero-delete-base.png" 2>/dev/null
  # The first `d` only opens the confirmation banner, pushing the total's
  # band down like the other prompts.
  "$WORK/xtest" "$DISP" key d
  sleep 1.5
  shot delete-confirm.png
  convert "$WORK/delete-confirm.png" -crop "$HERO" +repage "$WORK/hero-delete-confirm.png" 2>/dev/null
  DIFF=$(compare -metric AE "$WORK/hero-delete-base.png" "$WORK/hero-delete-confirm.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the delete confirmation painted ($DIFF pixels moved)"
  else
    bad "the delete confirmation painted" "the total's band is unchanged with the confirmation open"
  fi
  # Escape cancels: nothing is deleted.
  "$WORK/xtest" "$DISP" key Escape
  sleep 1.5
  check "escape cancels the delete" "3" "$("$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]')"
  check "a cancelled delete moves nothing" "02:15:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  # `d` again, then `d` to confirm: the hour goes away.
  "$WORK/xtest" "$DISP" key d
  sleep 1.5
  "$WORK/xtest" "$DISP" key d
  sleep 2.5
  check "d d deleted one entry" "2" "$("$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]')"
  check "the deleted hour left the total" "01:15:00" "$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')"
  "$CLI" list | grep -q smoke && bad "the deleted entry is gone" "smoke still listed" || ok "the deleted entry is gone"

  echo
  echo "== export from the GUI writes a file =="
  # Home -> Projects -> Export. The three green buttons there are the only
  # green on screen; they carry longer labels than the quick-add row, so the
  # width filter is wider than the quick-add one.
  "$WORK/xtest" "$DISP" key Tab
  sleep 1
  "$WORK/xtest" "$DISP" key Tab
  sleep 1.5
  shot export-tab.png
  EXPORTS=$(blobs export-tab.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45' | wc -l)
  check "three export buttons are drawn" "3" "$EXPORTS"
  # The anchor picker (scope pills, stepper, typed day) is neutral, never
  # action green, so the count above still holds with the picker painted.
  # One press back a period must repaint the tab (the period label moves),
  # and one press forward must bring it back pixel-identical.
  "$WORK/xtest" "$DISP" key bracketleft
  sleep 1.5
  shot export-prev.png
  DIFF=$(compare -metric AE "$WORK/export-tab.png" "$WORK/export-prev.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the anchor stepper repainted the tab ($DIFF pixels changed)"
  else
    bad "the anchor stepper repainted the tab" "the tab is pixel-identical after ["
  fi
  "$WORK/xtest" "$DISP" key bracketright
  sleep 1.5
  shot export-back.png
  DIFF=$(compare -metric AE "$WORK/export-tab.png" "$WORK/export-back.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" = "0" ]; then
    ok "stepping back and forth returned the tab"
  else
    bad "stepping back and forth returned the tab" "$DIFF pixels differ"
  fi
  # Scoped preview plus portal save: the preview shows the scoped rows
  # oldest first, so stepping to a week with no entries must empty it (the
  # tab repaints again), and saving there must write that week's
  # period-stamped file with the header alone. The dialog itself is stubbed
  # (see the launch env): the names it is offered are asserted after the
  # "Export all" save below.
  "$WORK/xtest" "$DISP" key bracketleft
  sleep 1.5
  shot export-empty-week.png
  DIFF=$(compare -metric AE "$WORK/export-tab.png" "$WORK/export-empty-week.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the scoped preview repainted for the empty week ($DIFF pixels changed)"
  else
    bad "the scoped preview repainted for the empty week" "the tab is pixel-identical with no rows in scope"
  fi
  # The leftmost button is "Export week": with last week's anchor it saves
  # that week's period-stamped name.
  CLICK=$(blobs export-empty-week.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  shot export-before-save.png
  "$WORK/xtest" "$DISP" click ${CLICK}
  for _ in $(seq 1 30); do [ "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)" = "1" ] && break; sleep 0.3; done
  check "the scoped save wrote one period-stamped week file" "1" "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)"
  PREV_WEEK_FILE=$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | head -1)
  check "the empty week file holds the header alone" "1" "$(wc -l <"$PREV_WEEK_FILE" 2>/dev/null)"
  check "the empty week file has the spec header" "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note" "$(head -1 "$PREV_WEEK_FILE" 2>/dev/null)"
  if [ -f "$WORK/timetrack-export-week.csv" ]; then
    bad "no un-stamped week file was written" "found $WORK/timetrack-export-week.csv"
  else
    ok "no un-stamped week file was written"
  fi
  # A save reports a status in the footer: the footer band must repaint.
  # The exact wording is pinned by unit test; pixels only prove it appeared.
  # Poll briefly: the repaint follows the service thread through the 300ms
  # poll loop, so one screenshot can beat it on slow software rendering.
  WINDOW_H=$(echo "$WINDOW" | awk '{print $2}')
  FOOT="$(echo "$WINDOW" | awk '{print $1}')x40+${WINDOW_X}+$(( WINDOW_Y + WINDOW_H - 40 ))"
  convert "$WORK/export-before-save.png" -crop "$FOOT" +repage "$WORK/foot-before.png" 2>/dev/null
  DIFF=0
  for _ in $(seq 1 20); do
    shot export-after-save.png
    convert "$WORK/export-after-save.png" -crop "$FOOT" +repage "$WORK/foot-after.png" 2>/dev/null
    DIFF=$(compare -metric AE "$WORK/foot-before.png" "$WORK/foot-after.png" null: 2>&1 | sed 's/ .*//')
    [ "${DIFF:-0}" != "0" ] && break
    sleep 0.5
  done
  if [ "${DIFF:-0}" != "0" ]; then
    ok "the save reported a status ($DIFF pixels changed)"
  else
    bad "the save reported a status" "the footer is pixel-identical after the save"
  fi
  # Back to the current week: saving again must land a second
  # period-stamped file, this one holding both remaining entries.
  "$WORK/xtest" "$DISP" key bracketright
  sleep 1.5
  shot export-current-week.png
  CLICK=$(blobs export-current-week.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  "$WORK/xtest" "$DISP" click ${CLICK}
  for _ in $(seq 1 30); do [ "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)" = "2" ] && break; sleep 0.3; done
  check "the current week saved a second period-stamped file" "2" "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)"
  CUR_WEEK_FILE=$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | grep -vF "$PREV_WEEK_FILE" | head -1)
  check "the current week file holds both remaining entries" "3" "$(wc -l <"$CUR_WEEK_FILE" 2>/dev/null)"
  # The rightmost button is "Export all".
  CLICK=$(blobs export-tab.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '3p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  "$WORK/xtest" "$DISP" click ${CLICK}
  for _ in $(seq 1 30); do [ -f "$WORK/timetrack-export-all.csv" ] && break; sleep 0.3; done
  if [ -f "$WORK/timetrack-export-all.csv" ]; then
    ok "the GUI export wrote a file"
  else
    bad "the GUI export wrote a file" "no $WORK/timetrack-export-all.csv after 9s"
  fi
  check "the GUI export has the spec header" "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note" "$(head -1 "$WORK/timetrack-export-all.csv" 2>/dev/null)"
  check "the GUI export holds both remaining entries" "3" "$(wc -l <"$WORK/timetrack-export-all.csv" 2>/dev/null)"
  # Portal wiring (ticket 27): the stub recorded the dialog's prefilled name
  # for each of the three saves above, and each must equal the period-stamped
  # file the fallback wrote.
  for f in "$PREV_WEEK_FILE" "$CUR_WEEK_FILE" "$WORK/timetrack-export-all.csv"; do
    contains "the dialog was offered $(basename "$f")" "$(basename "$f")" "$(cat "$WORK/portal-names.log" 2>/dev/null)"
  done
  # Portal edges (ticket 28, still stubbed, no live portal): the dialog
  # stays period-stamped throughout; overwrite needs only the portal's own
  # question, cancel is a quiet no-op that writes nothing, and a failed
  # write is a local error. Picker, preview, and fallback choice are
  # untouched: these pins only touch the `<log>.cancel`/`<log>.fail` switches.
  # Overwrite: saving the current week again offers the dialog once more and
  # overwrites the same fallback file in place (no second app confirm).
  LINES_BEFORE=$(wc -l <"$WORK/portal-names.log")
  shot export-before-overwrite.png
  CLICK=$(blobs export-before-overwrite.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  "$WORK/xtest" "$DISP" click ${CLICK}
  for _ in $(seq 1 30); do [ "$(wc -l <"$WORK/portal-names.log")" -gt "$LINES_BEFORE" ] && break; sleep 0.3; done
  check "an existing destination only asks the portal once more" "$((LINES_BEFORE+1))" "$(wc -l <"$WORK/portal-names.log" 2>/dev/null)"
  check "overwriting keeps one file per week" "2" "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)"
  check "the overwritten week file still holds both entries" "3" "$(wc -l <"$CUR_WEEK_FILE" 2>/dev/null)"
  contains "the overwrite dialog kept the period-stamped name" "$(basename "$CUR_WEEK_FILE")" "$(tail -1 "$WORK/portal-names.log" 2>/dev/null)"
  # Cancel: the next stubbed save answers Cancelled: the dialog is still
  # offered, no file appears, and the footer repaints with the quiet status
  # (the exact wording is pinned by unit test; pixels only prove it appeared
  # and was not an error-styled failure).
  touch "$WORK/portal-names.log.cancel"
  shot export-before-cancel.png
  convert "$WORK/export-before-cancel.png" -crop "$FOOT" +repage "$WORK/foot-before-cancel.png" 2>/dev/null
  WEEK_COUNT_BEFORE=$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)
  CLICK=$(blobs export-before-cancel.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  "$WORK/xtest" "$DISP" click ${CLICK}
  sleep 3
  check "cancel writes no new file" "$WEEK_COUNT_BEFORE" "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)"
  contains "cancel still offered the dialog" "$(basename "$CUR_WEEK_FILE")" "$(tail -1 "$WORK/portal-names.log" 2>/dev/null)"
  DIFF=0
  for _ in $(seq 1 20); do
    shot export-after-cancel.png
    convert "$WORK/export-after-cancel.png" -crop "$FOOT" +repage "$WORK/foot-after-cancel.png" 2>/dev/null
    DIFF=$(compare -metric AE "$WORK/foot-before-cancel.png" "$WORK/foot-after-cancel.png" null: 2>&1 | sed 's/ .*//')
    [ "${DIFF:-0}" != "0" ] && break
    sleep 0.5
  done
  if [ "${DIFF:-0}" != "0" ]; then
    ok "cancel left a quiet status ($DIFF pixels changed)"
  else
    bad "cancel left a quiet status" "the footer is pixel-identical after cancel"
  fi
  rm -f "$WORK/portal-names.log.cancel"
  # Failure: the next stubbed save grants an unwritable path: the dialog is
  # still offered, no fallback file appears, and the footer repaints with the
  # local error (path plus reason pinned by unit test, never service wording).
  touch "$WORK/portal-names.log.fail"
  shot export-before-fail.png
  convert "$WORK/export-before-fail.png" -crop "$FOOT" +repage "$WORK/foot-before-fail.png" 2>/dev/null
  CLICK=$(blobs export-before-fail.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK)"
  "$WORK/xtest" "$DISP" click ${CLICK}
  sleep 3
  check "a failed write leaves the week files alone" "2" "$(ls "$WORK"/timetrack-export-week-*.csv 2>/dev/null | wc -l)"
  contains "the failed dialog kept the period-stamped name" "$(basename "$CUR_WEEK_FILE")" "$(tail -1 "$WORK/portal-names.log" 2>/dev/null)"
  DIFF=0
  for _ in $(seq 1 20); do
    shot export-after-fail.png
    convert "$WORK/export-after-fail.png" -crop "$FOOT" +repage "$WORK/foot-after-fail.png" 2>/dev/null
    DIFF=$(compare -metric AE "$WORK/foot-before-fail.png" "$WORK/foot-after-fail.png" null: 2>&1 | sed 's/ .*//')
    [ "${DIFF:-0}" != "0" ] && break
    sleep 0.5
  done
  if [ "${DIFF:-0}" != "0" ]; then
    ok "a failed write reported a status ($DIFF pixels changed)"
  else
    bad "a failed write reported a status" "the footer is pixel-identical after the failed write"
  fi
  rm -f "$WORK/portal-names.log.fail"

  echo
  echo "== the window keeps serving while the save dialog is open =="
  # The save dialog used to be awaited on the service thread, so while it
  # was open the thread served nothing at all: no snapshot, no quick-add.
  # The window looked frozen over a frozen data set.
  #
  # The stub holds the dialog open for six seconds, which is long enough to
  # make a quick-add made during it either land or not. It fails the write
  # (the `.fail` flag), so nothing is written and the week files stay as
  # they are.
  touch "$WORK/portal-names.log.fail" "$WORK/portal-names.log.hold"
  TOTAL_BEFORE=$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')
  shot export-before-hold.png
  CLICK=$(blobs export-before-hold.png '#2ea043' | awk '$1 >= 60 && $2 >= 15 && $2 <= 45 {print $3" "$4" "$1" "$2}' | sort -n | sed -n '1p' | awk '{print int($1 + $3/2), int($2 + $4/2)}')
  echo "  clicking ($CLICK) and holding the dialog"
  "$WORK/xtest" "$DISP" click ${CLICK}
  sleep 1
  # The dialog is open (the stub is still holding it). A quick-add now must
  # still reach the service.
  "$WORK/xtest" "$DISP" key 1
  sleep 1
  TOTAL_DURING=$("$CLI" status | sed -nE 's/^all time:[[:space:]]+//p')
  if [ "$TOTAL_DURING" != "$TOTAL_BEFORE" ]; then
    ok "the service kept serving while the save dialog was open"
  else
    bad "the service kept serving while the save dialog was open" \
      "the total did not change ($TOTAL_BEFORE -> $TOTAL_DURING)"
  fi
  rm -f "$WORK/portal-names.log.fail" "$WORK/portal-names.log.hold"
  sleep 1

  echo
  echo "== a prompt opened on the Export tab is drawn there =="
  # The prompts used to be rendered inside the Home tab only, while the key
  # handler opened them on every tab. On the Export tab that produced a modal
  # that was never painted and that took the keyboard anyway: the window
  # looked frozen and only Escape got out.
  #
  # The prompt panel's own background (#1a1a26) is a big block that nothing
  # else on the Export tab uses, so its area is the signal: before the prompt
  # there is the titlebar's own strip, after it there is the titlebar plus the
  # panel.
  #
  # The shared `blobs` helper fuzzes 5%, which is wider than the distance
  # between the panel colour and the window background, so it cannot tell the
  # two apart. This one matches at 1% -- tight enough to separate them, loose
  # enough to absorb the rounding the GPU path does.
  panel_area() {
    convert "$WORK/$1" -fuzz 1% -fill white -opaque '#1a1a26' +fuzz -fill black +opaque white \
      "$WORK/panel-mask.png" 2>/dev/null
    convert "$WORK/panel-mask.png" -define connected-components:verbose=true -connected-components 8 null: 2>&1 |
      sed -nE 's/^ *[0-9]+: ([0-9]+)x([0-9]+)\+([0-9]+)\+([0-9]+) [^ ]+ ([0-9]+) gray\(255\)$/\5/p' |
      awk '{a += $1} END {print a+0}'
  }
  # The keystrokes below only reach the app if the pointer is over it: with
  # no window manager the input focus is PointerRoot.
  "$WORK/xtest" "$DISP" move $(( WINDOW_X + 20 )) $(( WINDOW_Y + 20 ))
  shot export-no-prompt.png
  BEFORE=$(panel_area export-no-prompt.png)
  "$WORK/xtest" "$DISP" key a
  sleep 1.5
  shot export-prompt.png
  AFTER=$(panel_area export-prompt.png)
  if [ "$AFTER" -gt "$(( BEFORE + 20000 ))" ]; then
    ok "the entry dialog painted on the Export tab ($(( AFTER - BEFORE )) pixels)"
  else
    bad "the entry dialog painted on the Export tab" \
      "the prompt panel's area barely changed ($BEFORE -> $AFTER)"
  fi
  # Escape must close it again, and the panel must go with it.
  "$WORK/xtest" "$DISP" key Escape
  sleep 1.5
  shot export-prompt-gone.png
  GONE=$(panel_area export-prompt-gone.png)
  if [ "$GONE" -lt "$(( BEFORE + 20000 ))" ]; then
    ok "escape closed the dialog on the Export tab"
  else
    bad "escape closed the dialog on the Export tab" \
      "the prompt panel is still there ($GONE pixels)"
  fi

  # Export -> Home, so the tab test below starts where it expects.
  "$WORK/xtest" "$DISP" key Tab
  sleep 1.5

  echo
  echo "== tab switching by keyboard =="
  shot tabs-home.png
  "$WORK/xtest" "$DISP" key Tab
  sleep 1.5
  shot tabs-away.png
  DIFF=$(compare -metric AE "$WORK/tabs-home.png" "$WORK/tabs-away.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" != "0" ]; then
    ok "tab switched away from home ($DIFF pixels changed)"
  else
    bad "tab switched away from home" "the window is pixel-identical after Tab"
  fi
  # Two more Tabs cycle Projects -> Export -> Home: the window must come
  # back pixel-identical, proving the cycle order as well as the keys.
  "$WORK/xtest" "$DISP" key Tab
  sleep 1.5
  "$WORK/xtest" "$DISP" key Tab
  sleep 1.5
  shot tabs-back.png
  DIFF=$(compare -metric AE "$WORK/tabs-home.png" "$WORK/tabs-back.png" null: 2>&1 | sed 's/ .*//')
  if [ "${DIFF:-0}" = "0" ]; then
    ok "three tabs cycled back to home"
  else
    bad "three tabs cycled back to home" "$DIFF pixels differ from the home shot"
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
