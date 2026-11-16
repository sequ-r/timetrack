#!/usr/bin/env bash
# Native launch smoke for the GUI (#157).
#
# The Windows/macOS CI matrix has no pixel rig: this starts a real service
# on a private socket, seeds one entry through the real CLI, launches the
# real GUI, and asserts the GUI process stays alive and logs its connect
# and first-frame markers. That proves the GUI starts, reaches the
# service, and builds a frame on the platform. Pixel truth stays with the
# Linux gui-smoke job, which this deliberately does not duplicate.
# Quick-add and suggestions clicks/keys are not driven here (no synthetic
# input rig on these runners); the per-platform manual matrix for them
# lives in the `gui-native` job comment in .github/workflows/ci.yml.
#
# Run it after
#   cargo build -p timetrack-gui -p timetrack-service -p timetrack-cli
# Debug is fine: nothing here measures pixels or paint.
#
# Needs a display: the CI runners have one. On a headless Linux box gpui
# opens no window and the first-frame marker never fires, which this
# reports as a failure of the environment, not of the app.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$ROOT/target/debug
GUI=$BIN/timetrack-gui
SVC=$BIN/timetrack-service
CLI=$BIN/timetrack
# Native binaries carry .exe on Windows (git-bash still runs them).
# Prefer the explicit .exe path: msys -x also succeeds for the bare
# name, and image-name tracking must probe the real OS image name
# ("timetrack-gui.exe"), not the extension-less msys alias.
[ -x "$GUI.exe" ] && GUI="$GUI.exe"
[ -x "$SVC.exe" ] && SVC="$SVC.exe"
[ -x "$CLI.exe" ] && CLI="$CLI.exe"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tt-gui-launch.XXXXXX")
# The store path is argv to a native binary, so on Windows it must be a
# native path: msys `/tmp/...` does not exist outside git-bash. (The
# socket override needs no conversion: on Windows only its file name is
# read, as the pipe name; elsewhere it is a real Unix path.)
STORE=$WORK/store.json
if command -v cygpath >/dev/null 2>&1; then
  STORE=$(cygpath -m "$WORK/store.json")
fi

pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  ok    %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  FAIL  %s\n' "$1"; [ $# -gt 1 ] && printf '        %s\n' "$2"; return 0; }

# Process handles. On Unix a background job's $! is the process itself.
# On Windows git-bash $! is an msys stub: the native child runs under a
# different Windows PID, so kill -0, taskkill /PID and tasklist's PID
# filter all address the stub, not the process (the stub lingers in the
# msys table, which is also why a bare `wait` wedges on it). The rig is
# hermetic -- one instance of each binary per runner, guarded below --
# so on Windows track processes by image name in the OS process table.
if command -v tasklist >/dev/null 2>&1; then WINDOWS=1; else WINDOWS=0; fi

proc_alive() { # $1 = handle: PID on Unix, image name on Windows
  if [ "$WINDOWS" = 1 ]; then
    tasklist //FI "IMAGENAME eq $1" //NH //FO CSV 2>/dev/null | grep -qi "\"$1\""
  else
    kill -0 "$1" 2>/dev/null
  fi
}

# taskkill exists only on Windows, where git-bash `kill` cannot be relied
# on to stop a native process. The doubled slashes stop msys from
# rewriting /F, /T and /IM into paths. /T takes any children with it,
# and a failure is reported, not swallowed: a rig that cannot stop its
# own processes must say so while the evidence is still on screen.
stop_proc() {
  if [ "$WINDOWS" = 1 ]; then
    local out
    if ! out=$(taskkill //F //T //IM "$1" 2>&1); then
      printf 'taskkill %s failed: %s\n' "$1" "$out" >&2
    fi
  else
    kill "$1" 2>/dev/null
  fi
}

cleanup() {
  [ -n "${GUI_HANDLE:-}" ] && stop_proc "$GUI_HANDLE"
  [ -n "${SVC_HANDLE:-}" ] && stop_proc "$SVC_HANDLE"
  if [ "$WINDOWS" = 1 ]; then
    # Never `wait` on the msys stubs: it can wedge forever even after
    # the native processes are gone, which used to turn a failed stop
    # into a 45-minute job timeout. Poll the OS process table for a
    # bounded grace period instead; the runner reaps any orphan.
    for _ in $(seq 1 10); do
      alive=0
      if [ -n "${GUI_HANDLE:-}" ] && proc_alive "$GUI_HANDLE"; then alive=1; fi
      if [ -n "${SVC_HANDLE:-}" ] && proc_alive "$SVC_HANDLE"; then alive=1; fi
      [ "$alive" = 0 ] && break
      sleep 1
    done
  else
    wait 2>/dev/null
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

for f in "$GUI" "$SVC" "$CLI"; do
  [ -x "$f" ] || {
    echo "missing $f -- run 'cargo build -p timetrack-gui -p timetrack-service -p timetrack-cli' first"
    exit 2
  }
done

# Image-name tracking is only sound if the rig owns every instance of
# its binaries: refuse to start beside a leaked one instead of probing
# the wrong process or killing a stranger's.
if [ "$WINDOWS" = 1 ]; then
  for im in "$(basename "$SVC")" "$(basename "$GUI")"; do
    if proc_alive "$im"; then
      echo "a $im instance is already running; the rig must own its processes" >&2
      exit 1
    fi
  done
fi

echo "== a seeded service on a private socket =="
# §16 hermeticity (mirrors e2e.sh): the rig service binds this socket and
# the GUI/CLI below can never reach a real running service.
export TIMETRACK_SOCKET="$WORK/tt.sock"

# TZ pins the service's offset, so day bucketing is deterministic.
TZ=UTC "$SVC" "$STORE" >"$WORK/svc.log" 2>&1 &
SVC_PID=$!
if [ "$WINDOWS" = 1 ]; then SVC_HANDLE=$(basename "$SVC"); else SVC_HANDLE=$SVC_PID; fi

# Poll through the CLI rather than grepping the log: a successful call is the
# real readiness signal, and it works identically after a restart.
ready=0
for _ in $(seq 1 90); do
  if "$CLI" status >/dev/null 2>&1; then ready=1; break; fi
  sleep 1
done
if [ "$ready" != 1 ]; then
  echo "service never became ready:"; cat "$WORK/svc.log"; exit 1
fi
ok "service is up"

# The service seeds a "General" project, so one quick-add leaves a store
# with a project and an entry for the GUI to render.
if out=$("$CLI" quick -m 15 2>&1); then
  case "$out" in *"added 00:15:00"*) ok "seeded one entry";; *) bad "seeded one entry" "$out";; esac
else
  bad "seeded one entry" "$out"
fi

echo "== the GUI launches against it =="
"$GUI" >"$WORK/gui.log" 2>&1 &
GUI_PID=$!
if [ "$WINDOWS" = 1 ]; then GUI_HANDLE=$(basename "$GUI"); else GUI_HANDLE=$GUI_PID; fi

# Wait for the first frame, refusing to outlive the process: a GUI that
# exits early (no display, a missing library) fails here with its log,
# not with a timeout that hides the cause.
framed=0
for _ in $(seq 1 90); do
  if grep -q "first frame rendered" "$WORK/gui.log" 2>/dev/null; then framed=1; break; fi
  if ! proc_alive "$GUI_HANDLE"; then break; fi
  sleep 1
done
if [ "$framed" = 1 ]; then
  ok "GUI rendered its first frame"
else
  if proc_alive "$GUI_HANDLE"; then
    bad "GUI rendered its first frame" "still running after 90s with no first-frame marker"
  else
    bad "GUI rendered its first frame" "GUI exited early -- the log follows"
  fi
fi

if grep -q "connected to the service" "$WORK/gui.log" 2>/dev/null; then
  ok "GUI connected to the service"
else
  bad "GUI connected to the service" "no connect marker in the GUI log"
fi

if proc_alive "$GUI_HANDLE"; then
  ok "GUI process is alive"
else
  bad "GUI process is alive" "GUI exited during the smoke check"
fi

# Stop the GUI and prove it stops: a wedged process would outlive the rig.
stop_proc "$GUI_HANDLE"
gone=0
for _ in $(seq 1 10); do
  if ! proc_alive "$GUI_HANDLE"; then gone=1; break; fi
  sleep 1
done
GUI_HANDLE=""
if [ "$gone" = 1 ]; then ok "GUI stopped on request"; else bad "GUI stopped on request" "process survived SIGTERM/taskkill"; fi

echo
echo "$pass passed, $fail failed"
if [ "$fail" -gt 0 ]; then
  echo "--- gui.log ---"; cat "$WORK/gui.log"
  echo "--- svc.log ---"; cat "$WORK/svc.log"
  exit 1
fi
