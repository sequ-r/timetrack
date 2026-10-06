#!/usr/bin/env bash
# End-to-end check for the manual time-tracking service.
#
# Runs a real service on a private session bus and drives it with the real CLI,
# so this exercises the same path a user takes.
#
# Two design choices keep this from being flaky:
#
#  - Totals are asserted as *deltas* after a known operation, not as absolute
#    sums. A week total depends on where "now" falls relative to the ISO week
#    boundary, so an absolute week assertion would fail every Sunday night and
#    pass every Tuesday. The all-time total has no such edge.
#  - Entry ids are extracted from the parenthesised form the CLI prints
#    (`(e12)`) rather than by grepping for `e<digits>`, which also matches the
#    digits inside other words and turns a one-id result into several.
set -uo pipefail

# Resolve the repository from the script's own location, so the script works
# from a clone anywhere rather than one hardcoded home directory.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SVC=$ROOT/target/release/timetrack-service
CLI=$ROOT/target/release/timetrack
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tt-e2e.XXXXXX")
STORE=$WORK/store.json

pass=0; fail=0
ok()   { pass=$((pass+1)); printf '  ok   %s\n' "$1"; return 0; }
bad()  { fail=$((fail+1)); printf '  FAIL %s\n' "$1"; [ $# -gt 1 ] && printf '       %s\n' "$2"; return 0; }
check(){ # check <desc> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected [$2] got [$3]"; fi
}
contains(){ # contains <desc> <needle> <haystack>
  case "$3" in *"$2"*) ok "$1";; *) bad "$1" "expected [$2] in [$(printf '%s' "$3" | head -3)]";; esac
}

# Entries created and removed so far, so the final counts can be asserted
# exactly instead of against a magic floor that a change could silently break.
created=0; removed=0

# The entry id the CLI printed as `(... e12)`.
id_of() { printf '%s' "$1" | grep -oE '\(e[0-9]+\)' | tr -d '()'; }

# The duration of a named entry. Matches the id as a whole field, so `e1` does
# not also match inside `e10`.
duration_of() {
  "$CLI" list | sed -nE "s/^.[[:space:]]+([0-9:]+)[[:space:]]+$1[[:space:]].*/\1/p"
}

# How many entries are listed.
listed() { "$CLI" list | grep -cE '[[:space:]]e[0-9]+[[:space:]]'; }

# The all-time total, as HH:MM:SS.
total() { "$CLI" status | sed -nE 's/^all time:[[:space:]]+//p'; }

cleanup() {
  [ -n "${SVC_PID:-}" ] && kill "$SVC_PID" 2>/dev/null
  wait 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== starting a private bus and the service =="
# A hermetic bus. `--session` reads standard_session_servicedirs, which
# includes ~/.local/share/dbus-1/services -- so a plain private bus
# auto-activates whatever host service is installed there, and that binary can
# win the name before ours does. A config with no service directories keeps
# this test talking only to the binary under test.
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

# `--fork` prints the address on stdout and then daemonises, so this returns
# immediately. (Asking for --print-pid as well makes it print two lines, and
# the address is no longer the only thing captured.)
DBUS_SESSION_BUS_ADDRESS=$(dbus-daemon --config-file="$BUS_CONF" --print-address --fork) || {
  echo "could not start a private bus"; exit 1; }
export DBUS_SESSION_BUS_ADDRESS
[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && { echo "empty bus address"; exit 1; }

# TZ pins the service's offset, so day bucketing is deterministic.
TZ=UTC "$SVC" "$STORE" >"$WORK/svc.log" 2>&1 &
SVC_PID=$!

# Poll through the CLI rather than grepping the log: a successful call is the
# real readiness signal, and it works identically after a restart.
wait_for_service() {
  for _ in $(seq 1 60); do
    "$CLI" status >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

wait_for_service || { echo "service never became ready:"; cat "$WORK/svc.log"; exit 1; }
echo "  service up: $(head -1 "$WORK/svc.log")"

# A fresh install must be usable immediately: the service seeds a project, so
# the very first command works with no setup. This replaced the old
# "with no projects, nothing can be added" check, which is no longer the
# behaviour -- and which never tested anything anyway, since it ran after a
# project already existed.
echo
echo "== a fresh store is usable with no setup =="
# The service seeds a "General" project, so the first command works with no
# prior setup. This replaced the old "with no projects, nothing can be added"
# check, which is no longer the behaviour and never tested anything anyway --
# it ran after a project already existed.
out=$("$CLI" quick -m 5)
contains "quick add works with no setup" "added 00:05:00" "$out"
FIRST=$(id_of "$out")
created=$((created+1))
# Prove the seed is really there by naming it, then undo the probe entry so the
# exact total assertions further down start from zero.
out=$("$CLI" add -p General -S -30 -E 0 -d "probe")
contains "the seeded project is selectable by name" "added 00:30:00" "$out"
created=$((created+1))
PROBE=$(id_of "$out")
"$CLI" delete "$FIRST" >/dev/null; removed=$((removed+1))
"$CLI" delete "$PROBE" >/dev/null; removed=$((removed+1))
check "both probe entries removed" "0" "$(listed)"

echo
echo "== projects =="
out=$("$CLI" project "Work"); contains "create Work" "created project" "$out"
out=$("$CLI" project "General" 2>&1); contains "duplicate name refused" "already exists" "$out"
out=$("$CLI" project "Home"); contains "create Home" "created project" "$out"
# p1 is the seeded General, p2 Work, p3 Home. The archive check below uses p2.

echo
echo "== projects rename and recolour =="
# A fourth project, so the renames below never disturb Work/Home/General
# and no later absolute total moves.
out=$("$CLI" project "RenameMe"); contains "create RenameMe" "created project" "$out"
out=$("$CLI" rename p4 Renamed)
contains "rename reports old and new" "renamed RenameMe to Renamed (p4)" "$out"
out=$("$CLI" recolour p4 2ea043)
contains "recolour reports the colour" "recoloured Renamed (p4) to #2ea043" "$out"
SNAP_JSON=$(gdbus call --session --dest org.sequ.timetrack \
  --object-path /org/sequ/timetrack \
  --method org.sequ.timetrack.Entries.Snapshot \
  | python3 -c 'import sys, ast; print(ast.literal_eval(sys.stdin.read().strip())[0])')
if AGREE=$(printf '%s' "$SNAP_JSON" | python3 -c '
import json, sys
snap = json.load(sys.stdin)
probs = []
ps = {p["id"]: p for p in snap["projects"]}
if ps.get("p4", {}).get("name") != "Renamed":
    probs.append("p4 not renamed")
if ps.get("p4", {}).get("colour") != 0x2EA043:
    probs.append("p4 not recoloured")
print(";".join(probs))
'); then
  check "rename and recolour reached the snapshot" "" "$AGREE"
else
  bad "rename and recolour reached the snapshot" "could not parse the snapshot"
fi
out=$("$CLI" rename p99 X 2>&1); contains "rename of an unknown id fails" "no project with id" "$out"
out=$("$CLI" rename p4 "" 2>&1); contains "blank rename refused" "cannot be blank" "$out"
out=$("$CLI" rename p4 Work 2>&1); contains "duplicate rename refused" "already exists" "$out"
out=$("$CLI" recolour p4 zzzzzz 2>&1); contains "bad hex refused" "RRGGBB" "$out"
out=$("$CLI" recolour p99 2ea043 2>&1); contains "recolour of an unknown id fails" "no project with id" "$out"

echo
echo "== set-text, set-project and unarchive =="
# One scratch entry for the editing commands: added and removed here, so
# no later absolute total moves.
out=$("$CLI" add -p Work -S -60 -E -30 -d "editable")
EE=$(id_of "$out"); created=$((created+1))
out=$("$CLI" set-text "$EE" "edited")
contains "text rewritten" "set text on $EE to \"edited\"" "$out"
if "$CLI" list | grep -E " $EE " | grep -q "edited"; then
  ok "new text reads back in the list"
else
  bad "new text reads back in the list" "edited missing"
fi
out=$("$CLI" set-project "$EE" Home)
contains "entry moved projects" "moved $EE to Home (p3)" "$out"
contains "export shows the new project" "$EE,Home," "$("$CLI" export --scope all | grep -E "^$EE,")"
out=$("$CLI" set-text e9999 "x" 2>&1); contains "set-text of an unknown id fails" "no entry with id" "$out"
out=$("$CLI" set-project e9999 Work 2>&1); contains "set-project of an unknown entry fails" "no entry with id" "$out"
out=$("$CLI" set-project "$EE" Nope 2>&1); contains "set-project onto an unknown project fails" "no project with id" "$out"
"$CLI" delete "$EE" >/dev/null; removed=$((removed+1))
check "scratch entry removed" "0" "$(listed)"
# Archive then unarchive p3: the snapshot must show both transitions, and
# the last one leaves history untouched.
out=$("$CLI" archive "p3")
contains "archived" "archived" "$out"
out=$("$CLI" unarchive "p3")
contains "unarchived" "unarchived Home (p3)" "$out"
SNAP_JSON=$(gdbus call --session --dest org.sequ.timetrack \
  --object-path /org/sequ/timetrack \
  --method org.sequ.timetrack.Entries.Snapshot \
  | python3 -c 'import sys, ast; print(ast.literal_eval(sys.stdin.read().strip())[0])')
if AGREE=$(printf '%s' "$SNAP_JSON" | python3 -c '
import json, sys
snap = json.load(sys.stdin)
probs = []
ps = {p["id"]: p for p in snap["projects"]}
if ps.get("p3", {}).get("archived", True) is not False:
    probs.append("p3 not unarchived")
print(";".join(probs))
'); then
  check "unarchive restored the project" "" "$AGREE"
else
  bad "unarchive restored the project" "could not parse the snapshot"
fi
out=$("$CLI" unarchive p99 2>&1); contains "unarchive of an unknown id fails" "no project with id" "$out"

echo
echo "== method 1: explicit start and end =="
out=$("$CLI" add -p Work -S -120 -E -60 -d "writing")
contains "explicit interval added" "added 01:00:00" "$out"
E1=$(id_of "$out"); created=$((created+1))
check "duration reads back as 1h" "01:00:00" "$(duration_of "$E1")"
check "all-time is 1h" "01:00:00" "$(total)"

echo
echo "== a backwards interval is refused and stores nothing =="
before=$(listed)
out=$("$CLI" add -p Work -S -60 -E -300 -d "backwards" 2>&1)
contains "backwards refused" "must not end before" "$out"
check "nothing was written" "$before" "$(listed)"

echo
echo "== method 2: duration ending now =="
out=$("$CLI" duration -p Work -f 30m -d "review")
contains "30m added" "added 00:30:00" "$out"
created=$((created+1))
check "all-time is 1h30m" "01:30:00" "$(total)"

echo
echo "== method 3: duration ending in the past =="
out=$("$CLI" past -p Work -f 90m -E -120 -d "meeting")
contains "90m in the past added" "added 01:30:00" "$out"
created=$((created+1))
check "all-time is 3h" "03:00:00" "$(total)"

echo
echo "== method 4: quick add, and its undo =="
out=$("$CLI" quick -p Work -m 15)   # `-m` is the short flag for --minutes
contains "quick add 15m" "added 00:15:00" "$out"
contains "undo hint printed" "undo with" "$out"
Q=$(id_of "$out"); created=$((created+1))
check "quick-add is marked with +" "1" "$("$CLI" list | grep -cE "^\+ .* $Q ")"

out=$("$CLI" quick -p Work -m 7 2>&1)
contains "bad bucket refused" "quick add takes one of" "$out"

out=$("$CLI" undo "$Q")
contains "undo removed it" "undid quick add" "$out"
removed=$((removed+1))
check "undo removed exactly the 15m it added" "03:00:00" "$(total)"
"$CLI" list | grep -qE " $Q " && bad "undone entry is gone" "still listed" || ok "undone entry is gone"

echo
echo "== undo refuses a hand-entered entry =="
out=$("$CLI" undo "$E1" 2>&1)
contains "undo guard" "not created by a quick add" "$out"
"$CLI" list | grep -qE " $E1 " && ok "hand-entered entry survived undo" || bad "hand-entered entry survived undo" "it was deleted"

echo
echo "== shortening keeps the entry =="
out=$("$CLI" shorten "$E1" -E -90)
contains "shortened" "shortened to" "$out"
check "shortened to 30m" "00:30:00" "$(duration_of "$E1")"
check "shortening removed exactly 30m" "02:30:00" "$(total)"
"$CLI" list | grep -qE " $E1 " && ok "entry still present after shortening" || bad "entry still present" "it was deleted"

echo
echo "== delete is explicit =="
out=$("$CLI" quick -p Home -m 5)
E3=$(id_of "$out"); created=$((created+1))
out=$("$CLI" delete "$E3")
contains "deleted" "deleted" "$out"
removed=$((removed+1))
"$CLI" list | grep -qE " $E3 " && bad "deleted entry is gone" "still listed" || ok "deleted entry is gone"
out=$("$CLI" delete "e9999" 2>&1); contains "deleting an unknown id fails" "no entry with id" "$out"

echo
echo "== overlap is counted twice, by design =="
"$CLI" add -p Work -S -240 -E -120 -d "a" >/dev/null; created=$((created+1))
"$CLI" add -p Work -S -180 -E -60  -d "b" >/dev/null; created=$((created+1))
# Two 2h entries overlapping by an hour sum to 4h. A wall-clock union would
# give 3h, and the requirement is explicitly the sum.
check "overlapping entries both counted" "06:30:00" "$(total)"

echo
echo "== the week total is reported and never exceeds the all-time total =="
out=$("$CLI" status)
contains "status reports the week" "this week:" "$out"
contains "status reports the month" "this month:" "$out"
week=$(printf '%s' "$out" | sed -nE 's/^this week:[[:space:]]+([0-9:]+).*/\1/p')
all=$(total)
[ -n "$week" ] && ok "week total present ($week)" || bad "week total present" "empty"
# A week is a subset of all time, so it can only be smaller or equal. Asserting
# the relationship rather than an absolute value is what makes this safe to run
# on any day of the week.
if [ "$(printf '%s\n%s\n' "$week" "$all" | sort | head -1)" = "$week" ]; then
  ok "week total is within the all-time total"
else
  bad "week total within all-time" "week=$week all=$all"
fi
# The month is a subset of all time, so the same relationship holds one
# level up. (Week vs month has no fixed order -- a week straddling the 1st
# holds days from last month -- so only the all-time bound is asserted.)
month=$(printf '%s' "$out" | sed -nE 's/^this month:[[:space:]]+([0-9:]+).*/\1/p')
[ -n "$month" ] && ok "month total present ($month)" || bad "month total present" "empty"
if [ "$(printf '%s\n%s\n' "$month" "$all" | sort | head -1)" = "$month" ]; then
  ok "month total is within the all-time total"
else
  bad "month total within all-time" "month=$month all=$all"
fi

echo
echo "== a day over 24h is flagged, not refused =="
for i in 1 2 3; do
  "$CLI" add -p Work -S -900 -E -60 -d "long $i" >/dev/null; created=$((created+1))
done
contains "24h warning shown" "more than 24h" "$("$CLI" status)"
# The point of the requirement: the data is kept even though it is implausible.
check "all three long entries were stored" "$((created-removed))" "$(listed)"

echo
echo "== split divides one entry in two, total unchanged =="
# Wall-clock moments, so every boundary is exact: relative moments drift by
# the seconds between two CLI calls, which would turn 00:30:00 into 00:29:59.
out=$("$CLI" add -p Work -S 09:00 -E 10:00 -d "splittable")
E_SPLIT=$(id_of "$out"); created=$((created+1))
out=$("$CLI" split "$E_SPLIT" --at 09:30)
contains "split reports both halves" "00:30:00" "$out"
H1=$(printf '%s' "$out" | grep -oE '\(e[0-9]+\)' | tr -d '()' | sed -n '1p')
H2=$(printf '%s' "$out" | grep -oE '\(e[0-9]+\)' | tr -d '()' | sed -n '2p')
created=$((created+1))
check "first half is 30m" "00:30:00" "$(duration_of "$H1")"
check "second half is 30m" "00:30:00" "$(duration_of "$H2")"
check "splitting changed nothing in the total" "49:30:00" "$(total)"

echo
echo "== merge fuses entries and states the delta =="
# The halves touch at exactly 09:30, so joining them is lossless.
out=$("$CLI" merge "$H1" "$H2" --yes)
contains "touching merge is lossless" "total unchanged" "$out"
removed=$((removed+1))
check "merged back to one hour" "01:00:00" "$(duration_of "$H1")"
check "total still 49:30" "49:30:00" "$(total)"
# Two overlapping hours: the union is 90 minutes, so merging states the
# 30-minute shrink up front (REQUIREMENTS §8) and the total drops by it.
out=$("$CLI" add -p Work -S 09:00 -E 10:00 -d "ov1"); OV1=$(id_of "$out"); created=$((created+1))
out=$("$CLI" add -p Work -S 09:30 -E 10:30 -d "ov2"); OV2=$(id_of "$out"); created=$((created+1))
check "two overlapping hours added" "51:30:00" "$(total)"
out=$("$CLI" merge "$OV1" "$OV2" --yes)
contains "merge states the shrink" "total shrinks by 00:30:00" "$out"
MOV=$(id_of "$out"); removed=$((removed+1))
check "overlapping merge dropped the 30m overlap" "51:00:00" "$(total)"

echo
echo "== split and merge refuse bad input without changing anything =="
before=$(listed)
out=$("$CLI" split "$E_SPLIT" --at 11:00 2>&1)
contains "split outside the entry refused" "split point is outside" "$out"
out=$("$CLI" split e9999 --at -90 2>&1)
contains "split of an unknown id fails" "no entry with id" "$out"
out=$("$CLI" merge "$E_SPLIT" --yes 2>&1)
contains "merge needs two entries" "need two" "$out"
out=$("$CLI" merge "$E_SPLIT" "$MOV" </dev/null 2>&1)
contains "merge without --yes refuses off-terminal" "re-run with --yes" "$out"
out=$("$CLI" quick -p Home -m 5); HX=$(id_of "$out"); created=$((created+1))
out=$("$CLI" merge "$E_SPLIT" "$HX" --yes 2>&1)
contains "merge across projects refused" "different projects" "$out"
"$CLI" delete "$HX" >/dev/null; removed=$((removed+1))
check "refusals stored nothing" "$before" "$(listed)"
check "counts still balance" "$((created-removed))" "$(listed)"

echo
echo "== persistence across a service restart =="
before_all=$(total)
before_n=$(listed)
kill "$SVC_PID"; wait "$SVC_PID" 2>/dev/null
TZ=UTC "$SVC" "$STORE" >>"$WORK/svc.log" 2>&1 &
SVC_PID=$!
wait_for_service || { echo "service did not come back"; cat "$WORK/svc.log"; exit 1; }
check "all-time total survived the restart" "$before_all" "$(total)"
check "every entry survived the restart" "$before_n" "$(listed)"

echo
echo "== ids do not collide after a restart =="
out=$("$CLI" add -p Work -S -30 -E 0 -d "after restart")
NEW=$(id_of "$out"); created=$((created+1))
# Whole-field match, so `e1` does not also match inside `e10`.
check "new id appears exactly once" "1" "$("$CLI" list | grep -cE " $NEW ")"
# And the counter continued rather than restarting from 1.
if [ "${NEW#e}" -gt 1 ]; then
  ok "id continued past the previous maximum ($NEW)"
else
  bad "id continued" "got $NEW, expected more than e1"
fi

echo
echo "== a named timezone restarts cleanly and keeps every total =="
# The service resolves TZ once at startup; a named zone must bucket through
# the tz database rather than falling back to UTC, without moving any total.
before_all=$(total)
before_n=$(listed)
kill "$SVC_PID"; wait "$SVC_PID" 2>/dev/null
TZ=Europe/Rome "$SVC" "$STORE" >>"$WORK/svc.log" 2>&1 &
SVC_PID=$!
wait_for_service || { echo "service did not come back on Europe/Rome"; cat "$WORK/svc.log"; exit 1; }
check "all-time total survived the timezone restart" "$before_all" "$(total)"
check "every entry survived the timezone restart" "$before_n" "$(listed)"
SNAP_JSON=$(gdbus call --session --dest org.sequ.timetrack \
  --object-path /org/sequ/timetrack \
  --method org.sequ.timetrack.Entries.Snapshot \
  | python3 -c 'import sys, ast; print(ast.literal_eval(sys.stdin.read().strip())[0])')
if AGREE=$(printf '%s' "$SNAP_JSON" | python3 -c '
import json, sys
snap = json.load(sys.stdin)
probs = []
if snap.get("tz") != "Europe/Rome":
    probs.append("tz is %r, not Europe/Rome" % snap.get("tz"))
if snap.get("local_offset_ms") not in (3_600_000, 7_200_000):
    probs.append("offset %r is not a Rome offset" % snap.get("local_offset_ms"))
if snap["all"]["total_ms"] != snap["total_ms"]:
    probs.append("aggregates disagree after the restart")
print(";".join(probs))
'); then
  check "the snapshot names its zone and agrees with itself" "" "$AGREE"
else
  bad "the snapshot names its zone and agrees with itself" "could not parse the snapshot"
fi
out=$("$CLI" status)
contains "status still reports the week on a named zone" "this week:" "$out"
contains "status still reports the month on a named zone" "this month:" "$out"
# Back to UTC for the remaining sections, so wall-clock assertions below
# run in the same zone as the entries were written in.
kill "$SVC_PID"; wait "$SVC_PID" 2>/dev/null
TZ=UTC "$SVC" "$STORE" >>"$WORK/svc.log" 2>&1 &
SVC_PID=$!
wait_for_service || { echo "service did not come back on UTC"; cat "$WORK/svc.log"; exit 1; }
check "all-time total survived the return to UTC" "$before_all" "$(total)"

echo
echo "== archiving keeps history in the totals =="
before_all=$(total)
out=$("$CLI" archive "p1")
contains "archived" "archived" "$out"
contains "history is preserved in the message" "stay in historical totals" "$out"
check "all-time unchanged by archiving" "$before_all" "$(total)"

echo
echo "== the store is authoritative and versioned =="
check "store format is the current version" '"version": 1' "$(sed -nE 's/^[[:space:]]*//p' "$STORE" | grep -m1 version | tr -d ',')"
check "no stopwatch state in the store" "0" "$(grep -cE '"(running|duration_ms)"' "$STORE")"

echo
echo "== CSV export writes a file with the spec columns =="
"$CLI" export --scope all --out "$WORK/export.csv" >/dev/null
contains "export header" "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note" "$(head -1 "$WORK/export.csv")"
contains "export has a data row" "manual" "$(cat "$WORK/export.csv")"
contains "export timestamps are ISO 8601" "T" "$(cat "$WORK/export.csv")"
# Week and month scopes write the same shape, narrowed: neither may hold
# more rows than the all-time export from the same store. (Week vs month
# has no fixed order -- a week straddling the 1st holds days from last
# month -- so only the all-time bound is asserted.)
"$CLI" export --scope week --out "$WORK/export-week.csv" >/dev/null
contains "week export header" "id,project,description,started_at,ended_at" "$(head -1 "$WORK/export-week.csv")"
"$CLI" export --scope month --out "$WORK/export-month.csv" >/dev/null
contains "month export header" "id,project,description,started_at,ended_at" "$(head -1 "$WORK/export-month.csv")"
all_rows=$(($(wc -l <"$WORK/export.csv") - 1))
week_rows=$(($(wc -l <"$WORK/export-week.csv") - 1))
month_rows=$(($(wc -l <"$WORK/export-month.csv") - 1))
if [ "$week_rows" -le "$all_rows" ] && [ "$month_rows" -le "$all_rows" ]; then
  ok "scoped exports narrow the rows (week $week_rows, month $month_rows, all $all_rows)"
else
  bad "scoped exports narrow the rows" "week=$week_rows month=$month_rows all=$all_rows"
fi
out=$("$CLI" export --scope bogus 2>&1); contains "bad scope refused" "unknown export scope" "$out"

echo
echo "== service aggregates agree on a large store =="
# The Projects tab reads its week and all-time columns from the snapshot's
# aggregates. If the all-time breakdown drifted from the entries, the tab
# would show disagreeing totals -- so grow the store past a handful of
# entries and compare the aggregates against the entries in the same
# snapshot. Uneven per-project durations keep the comparison honest.
for i in $(seq 1 15); do
  "$CLI" add -p Work -S -600 -E -540 -d "bulk work $i" >/dev/null; created=$((created+1))
  "$CLI" add -p Home -S -300 -E -270 -d "bulk home $i" >/dev/null; created=$((created+1))
done
SNAP_JSON=$(gdbus call --session --dest org.sequ.timetrack \
  --object-path /org/sequ/timetrack \
  --method org.sequ.timetrack.Entries.Snapshot \
  | python3 -c 'import sys, ast; print(ast.literal_eval(sys.stdin.read().strip())[0])')
if AGREE=$(printf '%s' "$SNAP_JSON" | python3 -c '
import json, sys
snap = json.load(sys.stdin)
probs = []
if snap["all"]["total_ms"] != snap["total_ms"]:
    probs.append("all.total_ms != total_ms")
if snap["all"]["entry_count"] != len(snap["entries"]):
    probs.append("all.entry_count != len(entries)")
sums = {}
for e in snap["entries"]:
    sums[e["project_id"]] = sums.get(e["project_id"], 0) + max(0, e["ended_at"] - e["started_at"])
if sums != snap["all"]["per_project"]:
    probs.append("per_project mismatch: %s vs %s" % (sums, snap["all"]["per_project"]))
print(";".join(probs))
'); then
  check "all-time aggregates agree with the entries" "" "$AGREE"
else
  bad "all-time aggregates agree with the entries" "could not parse the snapshot"
fi

echo
echo "== the D-Bus surface is the new one =="
INTRO=$(gdbus introspect --session --dest org.sequ.timetrack --object-path /org/sequ/timetrack 2>/dev/null)
check "interface is the new one" "org.sequ.timetrack.Entries" \
  "$(printf '%s' "$INTRO" | grep -o 'org\.sequ\.timetrack\.[A-Za-z]*' | sort -u | head -1)"
printf '%s' "$INTRO" | grep -qE '\b(Start|Stop|Cancel)\b' \
  && bad "stopwatch methods are gone" "Start/Stop/Cancel still exposed" \
  || ok "stopwatch methods are gone"
for m in Add AddDuration AddDurationEnding QuickAdd SetTimes SetText SetProject UndoQuickAdd DeleteEntry Split Merge AddProject UpdateProject SetArchived ExportCsv; do
  printf '%s' "$INTRO" | grep -q "$m" && ok "$m is exposed" || bad "$m is exposed" "not found"
done

echo
echo "======================================"
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ] || { echo; echo "--- store ---"; cat "$STORE"; echo; echo "--- service log ---"; cat "$WORK/svc.log"; }
exit $((fail > 0))
