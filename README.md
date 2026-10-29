# TimeTrack

[![ci](https://github.com/sequ-r/timetrack/actions/workflows/ci.yml/badge.svg)](https://github.com/sequ-r/timetrack/actions/workflows/ci.yml)

A time tracker for Linux. You **enter** time — a quick tap for the common case,
or an explicit interval when you are reconstructing the day — and the app tells
you where a week went.

There is no running timer. Every entry is a closed interval you either type in
or add with one of four buttons, and everything is summed: overlapping entries
count twice, because "how much time did I spend" is not "how much of my day did
it occupy".

The data lives in a **service**, not in either front end, so closing the GUI or
the terminal never loses anything, and both can be open at once.

## Architecture

```
  timetrack-gui (AppImage)          timetrack (CLI, in the AppImage)        timetrack-tray (host only)
  gpui 0.2.2, 3 tabs               ratatui TUI + one-shot commands     SNI item: week-total tooltip,
                                                                            per-project quick-add/undo/delete
        |                                  |                                   |
        +--------- D-Bus: org.sequ.timetrack.Entries ----+
                           |
              timetrack-service  (started by the AppImage)
              owns the store; does all aggregation
                           |
                  ~/.local/share/timetrack/store.json
```

| Crate | Role |
|---|---|
| `timetrack-core` | Model, rules, aggregation, atomic JSON storage. No GUI, IPC, async, **or clock**. |
| `timetrack-proto` | The D-Bus contract and a client. All front ends depend on this. |
| `timetrack-service` | D-Bus server. Owns the store, resolves the clock and timezone, persists on every change. |
| `timetrack-cli` | Terminal client: a ratatui TUI plus one-shot subcommands. |
| `timetrack-gui` | Desktop client, built on gpui (upstream, from crates.io). Ships as the AppImage. |
| `timetrack-tray` | Host-only tray item (StatusNotifierItem): service presence, week total, per-project menu. |
| `timetrack-update` | AppImage self-update check against GitHub releases; used by the GUI. |

The core takes an explicit timestamp and never reads the clock, so the rules are
identical everywhere and testable without any I/O. Aggregation is implemented in
the core but *applied* by the service, so the GUI and the CLI cannot disagree
about where a week starts.

## What works today

| | |
|---|---|
| Four ways to add time | explicit interval, duration ending now, duration ending earlier, quick-add (5/15/30/60) |
| Three ways to take it back | `shorten` moves an endpoint, `undo` removes a quick-add, `delete` is explicit |
| Split and merge | in the GUI (`s` split prompt, `m` merge confirm stating the shrink), the CLI, the core and the service |
| Totals | this ISO week, this month, all time, per project; the service aggregates so both UIs agree |
| Overlap | counted twice, by design; a day over 24h is flagged, never rejected |
| Projects | create, rename, archive, unarchive, recolour and delete (GUI `c`/`D` or the Projects rows' buttons; entries move projects from the edit dialog); archiving keeps the history in totals |
| Persistence | atomic writes; survives restart; corrupt or newer stores are refused, never silently reset |
| CSV export | current or chosen week/month (`--at YYYY-MM-DD`, default now) or all time; the CLI prints or writes a file, the GUI Export tab saves through the system dialog with period-stamped names |
| System tray | week-total tooltip with over-24h attention state, per-project quick-add / undo / delete-confirm, Open TimeTrack, Quit (host only) |

The GUI Export tab picks the period with a previous/next stepper (labelled
`2026-W41` / `2026-09`) plus a typed `YYYY-MM-DD` day, shows a preview of
exactly what the file will hold (scoped rows, oldest first), and saves through
the system save dialog — `timetrack-export-week-2026-W41.csv` and siblings by
default, so one week never overwrites the last. Cancelling the dialog does
nothing; without a portal it falls back to saving directly in your home
directory.

## Building

Requires Rust 1.85 or newer (the crates are edition 2024).

```sh
cargo build --release
# The tray's round-trip test spawns the debug service binary, which `cargo test`
# does not build (it compiles test harnesses, not binaries):
cargo build --package timetrack-service
cargo test --workspace --features timetrack-gui/portal-stub   # unit tests (GUI tests need the system libs below)
bash scripts/e2e.sh           # end-to-end checks against a real service
bash scripts/gui-smoke.sh     # headless GUI checks: paint, quick-add click, keyboard, confirm dialogs, tabs, export picker/preview/save
```

Coverage is informational, never a gate (#111). One-time setup
(`rustup component add llvm-tools-preview && cargo install cargo-llvm-cov --locked`),
then:

```sh
# Pre-build the binaries the tray round-trip test spawns into llvm-cov's
# target dir (it cleans that dir before building, hence --no-clean):
cargo build --package timetrack-service --package timetrack-tray --target-dir target/llvm-cov-target
cargo llvm-cov --no-clean --workspace --features timetrack-gui/portal-stub --summary-only
cargo llvm-cov report --html --output-dir coverage-html   # browsable per-line report
```

CI runs the same commands in the non-blocking `coverage` job and uploads the
HTML report as a run artifact. Baseline (2026-10-28): 68.6% lines overall —
core 96.3%, service 84.5%, update 90.5%, tray 76.6%, proto 68.0%, cli 51.2%,
gui 46.1% (the gpui render modules are exercised by the smoke check, not by
unit tests, so they read 0% here).

The `portal-stub` feature compiles in the save-dialog stub the GUI smoke
check drives: there is no live portal on a headless X server. A released
binary is built **without** it, so there is no stub in it to arm — the
system save dialog cannot be talked out of the way by an environment
variable.

CI runs those same commands on every pull request and on `main`
(`.github/workflows/ci.yml`: rustfmt, clippy, `cargo test --workspace`, then the
service rig and the GUI smoke check, each job on its own so a red run names the
one that failed). A failed smoke check uploads the screenshots and logs it took
(`TT_SMOKE_ARTIFACTS`) as a run artifact. Every push to `main` then runs
`release.yml`, which calls that workflow first and only afterwards mints the
run's version, builds the AppImage from it, tags the
commit and publishes the release. The version is the workspace manifest
bumped past the latest tag -- ordinary runs only ever move the patch, so to
ship a minor or major, bump `version` in the root `Cargo.toml` yourself and
the pipeline takes it as-is. Dispatching `release.yml` by hand runs the same
build and verification with `dry_run` on by default, so the packaging path
can be checked without cutting a release (or a tag).

On Linux the GUI additionally needs the system libraries gpui links against:

```sh
# Debian/Ubuntu
apt install libxkbcommon-dev libxkbcommon-x11-dev libfreetype-dev libfontconfig-dev
```

`gui-smoke.sh` runs the GUI on Xvfb, screenshots it and drives it through the
X11 XTest extension, so it needs no display of its own — but Xvfb implements
no DRI3 and Mesa's GPU drivers need it to present, so the GUI needs a software
Vulkan driver there (Arch: `vulkan-swrast`, Debian/Ubuntu:
`mesa-vulkan-drivers`; the script also accepts an extracted one via
`TT_LAVAPIPE_DIR`). Without it the window opens and stays black, which the
script reports as a driver problem rather than as a failure of the app.

The GUI's keyboard was the one thing headless checks could not reach: an Xvfb
server has no keymap, so every keysym lookup returns 0 and no keystroke can be
delivered. `setxkbmap` gives it one, so `gui-smoke.sh` now presses `1` and `q`
and asserts both. The window's root element still needs its id and `FocusHandle`
for `on_key_down` to fire at all — that requirement did not go away with the
toolkit.

## Running

The AppImage is the install. Launch it once, then any client — the image
starts its bundled service in the background, so there is no service to
install and no D-Bus activation file to write:

```sh
./TimeTrack-x86_64.AppImage          # desktop GUI (starts the service too)
./TimeTrack-x86_64.AppImage cli      # terminal client, same image
timetrack-tray &                     # system tray (host only)
```

For a bare `timetrack` on `PATH`, symlink it to the image — AppRun notices
the name it was called by and runs the CLI instead of the GUI:

```sh
ln -s /path/to/TimeTrack-x86_64.AppImage ~/.local/bin/timetrack
timetrack                            # TUI
timetrack quick -m 15                # or a one-shot command
timetrack status
```

Uninstall is deleting the AppImage file (and the symlink, if you made
one): the image writes nothing to the host — no desktop copy, no D-Bus
activation file, no systemd unit — so there is nothing left that can
dangle. Your tracked time lives outside the image at
`~/.local/share/timetrack/store.json` and is untouched by this.

Host `cargo build` binaries are a development fallback only (see
"Building" above): they need a service on the bus exactly like the tests
do, and they never take part in an install.

The TUI: `1`-`4` quick-add, `a` 30 minutes, `u` undo, `d` delete, `j`/`k` move,
`h`/`l` change project, `q` quit. Projects are created from the command line
(`timetrack project "Work"`), since the TUI has no text field yet.

The tray lists one submenu per unarchived project with its week total:
`+5m`…`+60m` quick-add rows, an `Undo quick-add` row (enabled only while this
session's last action on that project was a quick-add), and a
`Delete last entry (…)` confirm submenu stating duration and project. Its icon
shows the live week total as a tooltip and flattens with a start-service hint
when the service is away, or asks for attention on version skew and over-24h
days. `Open TimeTrack` launches the GUI through the desktop file's `Exec`;
`Quit` stops the tray only, never the service.

The tray needs a StatusNotifier watcher on the session bus: KDE, XFCE and
MATE provide one; on GNOME install a tray extension first, or the icon has
nowhere to appear. It runs on the host (never sandboxed), next to the
service.

### The tray has to be installed once

Same pattern as the service: a binary in `~/.local/bin`, the desktop file
in `~/.local/share/applications`, and a copy of it in `~/.config/autostart`
so it starts at login:

```sh
install -Dm755 target/release/timetrack-tray ~/.local/bin/timetrack-tray
install -Dm644 data/org.sequ.timetrack.tray.desktop ~/.local/share/applications/org.sequ.timetrack.tray.desktop
install -Dm644 data/org.sequ.timetrack.tray.desktop ~/.config/autostart/org.sequ.timetrack.tray.desktop
```

Bus-name ownership is unchanged by all of this: the tray connects anonymously
and never owns `org.sequ.timetrack`, so it can neither steal the service's
name nor keep a stale service alive — it only reads. The AppImage ships no
tray in v1 (it bundles the GUI, the service and the CLI); the tray is
host-only until it is decided whether it belongs in the image.

### Tray on GNOME, and why the AppImage ships none

GNOME Shell provides no StatusNotifier watcher, so the tray icon has nowhere
to appear there unless a tray extension (e.g. an AppIndicator extension)
registers one. KDE, XFCE and MATE ship a watcher, and the icon shows as-is.

The tray is host-only in v1, deliberately: it runs on the host next to the
service and is installed with the autostart file below. Until it is decided
whether the tray belongs inside the AppImage, run it from a host build.

### The service needs no install

The AppImage bundles the service and starts it in the background when the
app launches, so the session bus always finds one. There is no D-Bus
activation file and no systemd unit to install — and after deleting the
image there is none left behind to go stale, either.

**After upgrading, relaunch the image.** The interface was renamed from
`org.sequ.timetrack.Timer` to `org.sequ.timetrack.Entries` when the timer was
cut. A stale service binary keeps answering to the old name and the GUI reports
`UnknownInterface`. It now says so explicitly rather than claiming the service is
merely absent, because the two need different fixes:

> The GUI and the service are different versions. Reinstall whichever one is
> older so both speak `org.sequ.timetrack.Entries`.

With one artifact carrying both, that means: quit the old image (and its
leftover service, which exits with the session) and launch the new one.

## The terminal client

`timetrack` ships inside the AppImage — it is the same `timetrack-cli`
binary, at `usr/bin/timetrack` in the bundle. Run it through the image
(`./TimeTrack-x86_64.AppImage cli …`) or through a symlink named
`timetrack` pointing at the image (see "Running" above); either way the
image starts its bundled service first, so the CLI never needs a service
of its own.

### Commands

The four entry methods are four subcommands, and the three ways to take time
back are three *separate* commands, because they are three different operations:

```sh
timetrack add -p Work -S -90 -E -30 -d "review"   # method 1: explicit interval
timetrack duration -p Work -f 90m                 # method 2: ends now
timetrack past -p Work -f 90m -E -120             # method 3: ended earlier
timetrack quick -p Work -m 15                     # method 4: 5/15/30/60

timetrack shorten e4 -E -45                       # move an endpoint, keep the entry
timetrack undo e7                                 # only removes a quick-add
timetrack delete e4                               # explicit

timetrack export --scope week                     # this ISO week to stdout
timetrack export --scope week --at 2026-10-05 --out oct2.csv  # a chosen week to a file
timetrack export --scope month --at 2026-09-15    # a chosen month to stdout
timetrack export --scope all                      # everything (`--at` ignored)
```

Timestamps accept `-90` for "90 minutes ago" or `09:30` for that time today.
Durations accept `90m`, `1h30m` or `45s`; a bare number means minutes.

The asymmetry is deliberate: `shorten` can never delete anything, and `undo`
**refuses** an entry it did not create, so it cannot lose hand-entered time by
accident.

## AppImage

TimeTrack ships as a single portable file bundling `timetrack-gui`,
`timetrack-service` and the `timetrack` CLI, built with linuxdeploy +
appimagetool (fetched on demand). It needs no install and no service
setup: its AppRun starts the bundled service in the background, then the
GUI — or the CLI, when the image is invoked as `timetrack` (a symlink) or
with `cli` as the first argument. If a service is already running (a
second copy of the AppImage), the extra copy fails to own the bus name
and exits, and the client talks to whichever holds it.

```sh
./build-appimage.sh            # produces TimeTrack-<arch>.AppImage
chmod +x TimeTrack-x86_64.AppImage
./TimeTrack-x86_64.AppImage
```

Or by hand:

```sh
cargo build --release --package timetrack-gui --package timetrack-service --package timetrack-cli
# ... see build-appimage.sh for the AppDir layout and tool invocations
```

Release tags publish the AppImage, so one download covers the desktop app
and the terminal client on any Linux. The GUI's build needs the
same system libraries as above (`libxkbcommon-dev`,
`libxkbcommon-x11-dev`, `libfreetype-dev`, `libfontconfig-dev` on
Debian/Ubuntu); linuxdeploy bundles the linked ones into the image, so the
runtime needs nothing installed.

### Updates

The AppImage checks for updates itself, once per launch: it reads the
latest GitHub release and compares the tag against its own version. When a
newer release carries an image for your architecture, the footer says so
and offers `U`:

- first `U` arms the install (`press U again to download …`),
- second `U` downloads the new image and replaces the running file,
- relaunch to run it.

Nothing installs without those two presses, and an offline launch stays
silent rather than erroring. `TIMETRACK_NO_UPDATE=1` skips the check
entirely. Host builds (`cargo run`, installed binaries) never check —
there is no release to update to — and `U` there says so.

The image also carries embedded update information
(`gh-releases-zsync|…|TimeTrack-*.AppImage.zsync`), and releases publish
the `.zsync` file next to it, so the external
[AppImageUpdate](https://github.com/AppImage/AppImageUpdate) tool can
delta-update the same file. The in-app updater downloads the whole image
instead (~10 MB) and needs neither.

## Known issues

The things below are open, known, and not worked around. Everything else in
this README works today.

**The tray icon does not appear on GNOME.** GNOME Shell ships no
StatusNotifierItem watcher, so the icon has nowhere to go. Install a tray
extension (an AppIndicator one is the usual choice) or use the desktop
client. See "Tray on GNOME" above.

**The tray is not in the AppImage.** It is host-only for now: the image
bundles the GUI, the service and the CLI, and the tray is installed
separately with its autostart file. See "The tray has to be installed once"
above.

**A corrupt or newer store is refused, not repaired.** The service will
not start rather than start empty, because starting empty would look like
your tracked time had vanished. The file is at
`~/.local/share/timetrack/store.json`; move it aside to start fresh. A
file written by a newer TimeTrack is refused the same way, and says so.

**A store from the stopwatch era cannot be migrated.** That format has no
projects, so there is nothing to migrate it onto. The service says so
plainly and refuses it; move the file aside to start fresh.

**The TUI cannot create a project.** It has no text field for one; create
it from the command line (`timetrack project "Work"`) and it appears in
the TUI at the next refresh.

**The GUI's export preview shows the first rows, not the whole file.** It
shows up to twenty rows, oldest first, in exactly the order the file
carries them — enough to check the scope, not enough to read the file.

## Development

This section merges the former `PLAN.md` and `TODO.md`, both derived from
`REQUIREMENTS.md`. `REQUIREMENTS.md` remains the agreed specification
(§15 tracks the same gap from the specification's side); what follows is
the build order, the history and the traps worth remembering.

The ordering rule was **core before service, service before UI**, each
increment leaving the app working and independently verifiable. The core
holds the rules and has no I/O, so it can be tested exhaustively and
cheaply. The service aggregates so both UIs agree. The UIs came last
because they are where requirements are cheapest to change. All four
increments are now built.

### Increment 1 — core model and rules (`timetrack-core`)

- **Model** (`model.rs`): `Entry { id, project_id, description,
  started_at, ended_at: i64, source, note }` with `ended_at` non-optional
  (the timer is gone, so `duration_ms()` takes no `now`), `Project { id,
  name, colour, archived }`, `EntrySource { Manual, QuickAdd }`, and a
  version field on `Store` for future migrations to branch on.
- **Rules** (`rules.rs`): `create_entry` (arbitrary interval, overlap
  allowed), `quick_add` (5/15/30/60 ending at now, a new entry each
  time), `set_times` (move either endpoint, reversible, never deletes),
  `set_project`, `set_text`, `delete_entry`, and `split` / `merge`.
  Merge takes the union of the intervals, so merging overlapping
  entries shrinks the summed total. The only validation is that an
  entry may not end before it starts; overlap is never an error.
- **Aggregation** (`aggregate.rs`): sums of durations per project and
  overall over ISO weeks (Monday start) and calendar months in local
  time. Timezone conversion happens here and nowhere else. A day over
  24h of summed time is flagged, never rejected.
- **Storage** (`storage.rs`): atomic saves (sibling temp file, `fsync`,
  rename), so a crash mid-write leaves the previous good file. A
  corrupt or too-new store is an error, never a silent reset.

The rules take an explicit timestamp and stay deterministic, which is
how the overlap, merge and week-boundary edge cases are pinned down.
Four bugs were found this way that each looked fine in review:
`local_day_of` had the offset sign inverted; `month_of` and
`iso_week_of` returned 1970 for a 2024 date; `quick_add` delegated to a
helper that hardcoded `EntrySource::Manual`, making every quick-add
un-undoable; and `split_entry` wrote its first half to a temporary that
was never stored.

### Increment 2 — service (`timetrack-service`)

The service stores the model with no `running` concept and no
start/stop/cancel methods or `AlreadyRunning` / `NotRunning` errors: the
interface is `org.sequ.timetrack.Entries`, with D-Bus methods for entry
CRUD, projects and split/merge. `Snapshot` carries the week, month and
all-time aggregates plus the over-24h day list, so both UIs read the
same totals. Two details worth knowing: a genuinely empty store seeds
one `General` project (otherwise every quick-add was inert until the
user made a project, and an archived-only store is never re-seeded
over), and the service holds exactly one clock read and one timezone
resolution, so every `ended_at` traces to a single instant per call.

Note for the test rig: `scripts/e2e.sh` drives a real service over a
private bus with an explicit bus config, because a plain
`dbus-daemon --session` reads `standard_session_servicedirs` and
auto-activates whatever service is installed on the host — a stale
binary can then win the name and look exactly like a test failure.

### Increment 3 — GUI (`timetrack-gui`)

The Home tab leads with the week total at 72px (sized explicitly; the
largest step in gpui's scale was too small), quick-add buttons, undo,
and per-project week totals sorted longest first. The manual-entry
dialog (`a` new, prefilled with end = now; `e` edit, where an empty
time keeps the current endpoint) covers the two explicit entry
methods with one form; its spellings are shared with the CLI in
`core/src/parse.rs`.

Two gpui traps, both easy to regress: `Entity::update` never repaints
on its own — only `cx.notify()` schedules a frame — so every mutation
site notifies, and the poll loop notifies only when it drained a real
change instead of repainting several times a second. And key events
dispatch along the focused node's ancestor path, so the root element
needs an id and a `FocusHandle` with focus on the first frame, or the
keyboard is dead while click handlers (such as the tabs) still work.
The headless rig in `gui-smoke.sh` adds two traps of its own, covered
under "Building" above: Xvfb needs `setxkbmap` before any keysym
resolves, and with no window manager the pointer must be over the
window (PointerRoot focus) for a keystroke to arrive at all.

The GUI originally used **gpui-ce**, the community fork, because the
published `gpui` crate then carried no platform backend. Upstream
0.2.2 ships the Linux backends behind its default features, so
`gpui = "0.2.2"` from crates.io replaced both git dependencies. Exactly one call site
changed — `FocusHandle::focus(window, cx)` lost its `cx` argument —
plus an `image` pin that only existed for the fork's Wayland backend.
The lockfile moved the other way: the fork was *ahead* of upstream on
several UI dependencies (wgpu 29, cosmic-text 0.19 against blade-graphics
and cosmic-text 0.14), so those came back down to upstream's pins.
Nothing here depends on either; the GUI uses boxes, text, colour and
click handlers only.

### Increment 4 — CLI (`timetrack-cli`)

One subcommand per entry method (see "The terminal client" above). The
three removal operations stay three separate subcommands —
`shorten`, `undo`, `delete` — because they are three different things
to do to an entry, and collapsing them would hide the distinction the
specification is precise about.

### Completed checklist

The former `TODO.md` (from `REQUIREMENTS.md` §15, the plan's remaining
work and a code-health survey of 2026-10-05) is fully done. Recorded
here so the decisions and their tests stay findable; current behaviour
is described under "What works today" above.

*Spec gaps (2026-10-05/06):*

1. **CSV export end-to-end** — `core/src/export.rs`, service `ExportCsv`,
   CLI `timetrack export --scope week|month|all [--out FILE]`, GUI
   Export tab. Header row, ISO-8601 local timestamps, duration as
   `HH:MM:SS` plus minutes, `source` column.
2. **GUI manual-entry dialog** — `a` / `e` with the shared spellings in
   `core/src/parse.rs` and the `SetText` D-Bus method; landed together
   with the `cx.notify()` refresh fix described above.
3. **Per-instant timezone lookup** — `core/src/tz.rs` (`Tz` over
   `chrono-tz`, fixed-point `day_start`); aggregation, rules, export
   and parsing all take `&Tz`, the service resolves it once
   (`Tz::from_env`) and snapshots carry the zone name for clients
   (`Tz::from_snapshot`). Before this, named zones such as
   `Europe/Rome` fell back to UTC and mis-bucketed near DST and week
   boundaries; DST transition and week-split tests now pin it down.

*Correctness and data safety (2026-10-06):*

4. **Persist-failure path** — a failed save now returns the error and
   keeps the in-memory state (it previously reloaded with
   `unwrap_or_default()`, silently resetting to empty); the next
   successful save heals a transient failure.
5. **Projects totals agreement** — the snapshot carries a
   service-computed all-time `TotalsView` (`core::all_totals`), so the
   Projects tab no longer recomputes all-time totals from the
   recent-entries window while the week came from the service.
6. **Typed D-Bus errors** — `proto::ServiceError`
   (`Rule` / `UnknownExportScope` / `Storage` / `Other`) travels as
   JSON in the error body and `proto::ClientError`
   (`Service` / `NoService` / `VersionMismatch` / `Transport`) triages
   by D-Bus error name, so the GUI tells "no service" from "wrong
   service" from a validation failure without parsing strings. The
   user-visible wordings stayed byte-identical.

*Reachable features and polish (2026-10-06):*

7. **Split/merge in the GUI and CLI** — GUI `s` / `m`, CLI
   `split ID --at MOMENT` and `merge IDS... [--yes]`, with
   `merge_shrink_ms` / `format_merge_delta` shared from the core so
   both UIs state the shrink identically.
8. **Project naming** — `core::update_project` (with a
   `BlankProjectName` guard), D-Bus `UpdateProject` (`""` / `-1` keep
   sentinels, since D-Bus has no `Option`), CLI `rename` / `recolour`
   (strict `RRGGBB`), and the GUI naming prompt (`n` / `r`) that
   replaced invented `Project N` names.
9. **CLI parity** — `set-text`, `set-project` and `unarchive`
   subcommands, including a new `SetProject` service path that the
   original checklist scope had missed (no such method existed).
10. **Tab shortcuts and proportional bars** — `Tab` / `Shift+Tab`
    cycles the three tabs (the entry dialog keeps `Tab` for its
    fields); per-project week rows draw a bar proportional to the
    longest row.
11. **Confirmations** — delete and merge ask first (merge states the
    shrink amount), and archiving is reversible via un-archive.

*Engineering hygiene:*

12. **Clippy and formatting gated in CI** — the workspace is clean
    under `cargo clippy --workspace -- -D warnings` and
    `cargo fmt --check`, and CI enforces both.
13. **E2e / GUI-smoke coverage** — `scripts/e2e.sh` and
    `scripts/gui-smoke.sh` cover split/merge, the `set_*` guards,
    month navigation, export, undo/delete, tabs and total read-back,
    plus service DST and empty-store tests (see "Building").

### Deferred, deliberately

- **Drag-to-trim** for adjusting entry times.
- **A dedicated entry-list tab** — editing lives behind the per-project
  rows until it needs more room.

### Risks worth remembering

- **Timezone handling in aggregation** is the most likely source of
  off-by-one bugs, particularly around DST and month boundaries, which
  is why the ISO-week rule lives in exactly one place. Per-instant
  lookup (item 3 above) fixed the known case; the residual risk is
  zones with midnight transitions and skipped `HH:MM` inputs on
  spring-forward days, which are mapped arithmetically rather than
  rejected.
- **Merge reducing the total** looks like a bug to anyone who has not
  read the confirmation text. Both UIs state the delta, worded by the
  shared `rules.rs::format_merge_delta` so they cannot drift.
- **A stale service binary outliving an interface rename** cost the
  most time in an early round: a GUI speaking `.Entries` against a
  service still answering to `.Timer` looks like a dead app, not a
  version mismatch. The GUI now names the problem; see "The service
  needs no install" above for the fix.

## Licence

GPL-3.0-or-later. See `COPYING`.
