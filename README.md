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

## Download

The AppImage is the install: one portable file, no dependencies, no setup.
Take it from [the latest release](https://github.com/sequ-r/timetrack/releases/latest)
(`TimeTrack-x86_64.AppImage`, next to its `.zsync` and `SHA256SUMS`):

```sh
chmod +x TimeTrack-x86_64.AppImage
./TimeTrack-x86_64.AppImage
```

See [Running](#running) for the terminal and tray modes, and
[Updates](#updates) for how the image updates itself in place.

## Architecture

```
  timetrack-gui (AppImage)          timetrack (CLI, in the AppImage)        timetrack-tray (in the AppImage)
  gpui 0.2.2, 3 tabs               one-shot commands only (§18)      SNI item: week-total tooltip,
                                                                            per-project quick-add/undo/delete
        |                                  |                                   |
        +--- local socket (typed contract, §16) --+
                           |
              timetrack-service  (started by the AppImage, or auto-spawned by a client)
              owns the store; does all aggregation
                           |
                  ~/.local/share/timetrack/store.db (SQLite)
```

| Crate | Role |
|---|---|
| `timetrack-core` | Model, rules, aggregation (and the legacy JSON reader the one-time import uses). No GUI, IPC, async, **or clock**. |
| `timetrack-store` | Embedded SQLite persistence (§17/ADR 0014): schema migrations, the JSON auto-import, sync metadata/tombstones. Only the service depends on it. |
| `timetrack-proto` | The wire contract and a client: the local-socket transport (§16). All front ends depend on this. |
| `timetrack-service` | Socket server. Owns the store, resolves the clock and timezone, persists on every change. |
| `timetrack-cli` | Terminal client: one-shot subcommands only (the TUI was removed in the v2 rework). |
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
| System tray | week-total tooltip with over-24h attention state, per-project quick-add / undo / delete-confirm, Open TimeTrack, Quit |

The GUI Export tab picks the period with a previous/next stepper (labelled
`2026-W41` / `2026-09`) plus a typed `YYYY-MM-DD` day, shows a preview of
exactly what the file will hold (scoped rows, oldest first), and saves through
the system save dialog — `timetrack-export-week-2026-W41.csv` and siblings by
default, so one week never overwrites the last. Cancelling the dialog does
nothing; without a portal it falls back to saving directly in your home
directory.

## Building

Requires Rust 1.99 or newer (the crates are edition 2024).

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
HTML report as a run artifact. Baseline (2026-11-04): 63.9% lines overall —
core 95.7%, service 73.2%, update 90.5%, tray 76.0%, proto 65.8%, cli 53.9%,
gui 40.9% (the gpui render modules are exercised by the smoke check, not by
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
(`TT_SMOKE_ARTIFACTS`) as a run artifact. A push to `main` that touches the
app (`crates/`, the manifests, the lockfile, `build-appimage.sh`, `data/`,
`scripts/mint-version.sh`) then runs `release.yml`, which calls that workflow
first and only afterwards mints the run's version, builds the AppImage from
it, tags the commit and publishes the release. Anything else runs the checks
but mints no version. The version is the workspace manifest
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
starts its bundled service and tray in the background, so there is no
service to install and no D-Bus activation file to write:

```sh
./TimeTrack-x86_64.AppImage          # desktop GUI (starts service + tray too)
./TimeTrack-x86_64.AppImage cli      # terminal client, same image
./TimeTrack-x86_64.AppImage tray &   # tray in the foreground (autostart use)
```

For a bare `timetrack` on `PATH`, symlink it to the image — AppRun notices
the name it was called by and runs the CLI instead of the GUI:

```sh
ln -s /path/to/TimeTrack-x86_64.AppImage ~/.local/bin/timetrack
timetrack                            # status: this week's total first
timetrack quick -m 15                # a one-shot command
timetrack status --json              # the same totals, for scripts
```

Uninstall is deleting the AppImage file (and the symlink, if you made
one): the image writes nothing to the host — no desktop copy, no D-Bus
activation file, no systemd unit — so there is nothing left that can
dangle. Your tracked time lives outside the image at
`~/.local/share/timetrack/store.db` (SQLite, §17) and is untouched by
this. To back up or move it, stop the service first and copy `store.db`
plus its `store.db-wal` sibling together. Upgrading from the JSON era
needs nothing: the first start imports `store.json` (plus its journal)
into the database and leaves both files untouched as your backup.

Host `cargo build` binaries are a development fallback only (see
"Building" above): they need a service on the bus exactly like the tests
do, and they never take part in an install.

The CLI is one-shot only (§18): `timetrack quick -m 15` between other
terminal work, `timetrack status` for the glance, `timetrack project
"Work"` to create a project. There is no TUI any more — interactive
editing is the GUI's job.

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

### Running the tray

The tray ships inside the AppImage and starts with the app: launching the
GUI (or the CLI) starts it in the background next to the service, and it
stays after the window closes until you pick its `Quit` row. There is only
ever one: the tray owns `org.sequ.timetrack.tray` on the session bus for
its lifetime — the same single-instance trick as the service — so starting
the app again never stacks a second icon, and an extra copy just exits.

For the icon at login, before you have launched anything, add the tray to
your desktop's startup applications (the image itself writes no autostart
file for you). The `tray` mode runs it in the foreground for exactly that:

```sh
./TimeTrack-x86_64.AppImage tray &
# or symlink it like the CLI, and run the bare name:
ln -s /path/to/TimeTrack-x86_64.AppImage ~/.local/bin/timetrack-tray
timetrack-tray &
```

From a host build it installs the same way as before — a binary in
`~/.local/bin`, the desktop file in `~/.local/share/applications`, and a
copy in `~/.config/autostart`:

```sh
install -Dm755 target/release/timetrack-tray ~/.local/bin/timetrack-tray
install -Dm644 data/org.sequ.timetrack.tray.desktop ~/.local/share/applications/org.sequ.timetrack.tray.desktop
install -Dm644 data/org.sequ.timetrack.tray.desktop ~/.config/autostart/org.sequ.timetrack.tray.desktop
```

Service bus-name ownership is unchanged by all of this: the tray connects
to the service anonymously and never owns `org.sequ.timetrack`, so it can
neither steal the service's name nor keep a stale service alive — it only
reads. The one name it does own is its own, `org.sequ.timetrack.tray`,
held purely so a second launch exits instead of stacking icons.

### Quick-add without a tray (stock GNOME)

GNOME Shell provides no StatusNotifier watcher, so on stock GNOME the tray
icon has nowhere to appear. The tray does not sit there silently: it logs
why, posts one desktop notification saying so, and keeps waiting in case a
watcher shows up (an AppIndicator extension provides one, and the icon then
appears without restarting anything). Quick-add itself does not need the
tray on GNOME:

- **Launcher actions:** right-click the TimeTrack icon in the dash or app
  grid and pick *Add 5/15/30/60 minutes*. Time lands on your first active
  project and a notification confirms how much and where.
- **In the app:** keys `1`-`4` on the Home tab add a bucket to the selected
  project.
- **A global shortcut you bind yourself:** in GNOME Settings → Keyboard →
  Custom Shortcuts, bind a key to
  `/path/to/TimeTrack-x86_64.AppImage cli quick -m 15 --notify` (add
  `-p Work` to pin a project). That is a one-keystroke quick-add from any
  app, no extension involved.

### The service needs no install

The AppImage bundles the service and starts it in the background when the
app launches; a client that finds nothing on the service socket spawns
the bundled service itself. There is no systemd unit and no activation
file to install — and after deleting the image there is none left
behind to go stale, either.

**After upgrading, relaunch the image.** The service speaks one socket
protocol now (the D-Bus interfaces it used to dual-serve are retired,
ADR 0017), so quit the old image — and its leftover service, which
exits with the session — and launch the new one. A stale pre-socket
service simply never answers on the socket and the clients report it
as not running.

### Timezone

The service is the only timezone authority: it resolves its zone from `TZ`
(or the system timezone) at startup, reports the zone's name with every
snapshot, and the GUI and CLI read it from there — they never look at their
own `TZ`. That is what keeps a typed `09:30` and the service's week and
month totals in the same local day, DST transitions included.

- **Travelling needs no restart.** The service re-reads the system timezone
  continuously, so changing it (GNOME Settings, `timedatectl set-timezone`)
  takes effect within a second or two. Changing the `TZ` environment
  variable is the one exception: a process's environment is fixed at
  launch, so relaunch the AppImage for a new `TZ` to reach its service.
- **An unusable zone stops the service at startup** with a message naming
  the bad value, instead of silently tracking time in UTC. Fix `TZ` (an
  IANA name such as `Europe/Rome`, `UTC`, or `UTC+2`) or the system zone
  and launch again. A client talking to a service too old to report a zone
  name likewise refuses to parse typed times rather than guess.
- **Clock-change days:** a wall-clock time the spring-forward jump skips
  (e.g. `02:30` on the day 02:00 becomes 03:00) is refused, because it
  never occurs; a time in the repeated fall-back hour means its first
  occurrence.

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

### No TUI

Running `timetrack` with no subcommand prints the status and exits:
this week's total first, then the month and all-time totals. The
full-screen TUI was removed in the v2 rework (§18, ADR 0016) — the
terminal client is for one-shot commands between other work, and every
former TUI action is a subcommand above (`add`, `quick`, `undo`,
`project`, `export`, …). `status --json` and `list --json` print the
same data as JSON for scripts and status bars.

## AppImage

TimeTrack ships as a single portable file bundling `timetrack-gui`,
`timetrack-service`, the `timetrack` CLI and `timetrack-tray`, built with linuxdeploy +
appimagetool (fetched on demand). It needs no install and no service
setup: its AppRun starts the bundled service and tray in the background,
then the GUI — or the CLI, when the image is invoked as `timetrack` (a
symlink) or with `cli` as the first argument. If a service (or the tray)
is already running (a second copy of the AppImage), the extra copy finds
the socket (or its tray name) owned and exits; clients talk to the owner.

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

**The tray icon does not appear on stock GNOME.** GNOME Shell ships no
StatusNotifierItem watcher, so the icon has nowhere to go; the tray says
so in a notification rather than running invisibly. Quick-add on GNOME is
the launcher's right-click actions (or a tray extension restores the
icon). See "Quick-add without a tray (stock GNOME)" above.

**A corrupt or newer store is refused, not repaired.** The service will
not start rather than start empty, because starting empty would look like
your tracked time had vanished. The store is
`~/.local/share/timetrack/store.db` (with its `-wal` sibling while the
service runs); move it aside to start fresh. A database written by a
newer TimeTrack is refused the same way, and says so. A pre-SQLite
`store.json` found beside it is only ever read by the one-time import.

**A store from the stopwatch era cannot be migrated.** That format has no
projects, so there is nothing to migrate it onto. The service says so
plainly and refuses it; move the file aside to start fresh.

**The GUI's export preview shows the first rows, not the whole file.** It
shows up to twenty rows, oldest first, in exactly the order the file
carries them — enough to check the scope, not enough to read the file.

## Contributing

Issues live on [GitHub](https://github.com/sequ-r/timetrack/issues); the
[`gh` CLI](https://cli.github.com/) is how to work with them. A good issue
states what you saw, what you expected, and how to reproduce it. Triage
moves through five labels: `needs-triage`, `needs-info`,
`ready-for-agent` / `ready-for-human`, `wontfix`.

Work happens on a branch named `<kind>/<what>` (`fix/…`, `feat/…`,
`chore/…`) and lands through a pull request against `main`. One PR, one
change; keep it small enough to review in one sitting.

`REQUIREMENTS.md` is the agreed specification: read it before adding
behaviour, and flag any contradiction instead of silently overriding it.
Use the words in `GLOSSARY.md` (entry, project, export, … ― never
interval, tag, report) and read the `docs/adr/` notes touching your area;
their decisions stand unless explicitly reopened.

Before opening the PR, run what CI runs (see "Building" above): rustfmt,
clippy with `-D warnings`, the workspace tests, and the e2e / GUI rigs if
you touched their area. Sign your commits.

## Repository rules

`main` is protected: no force-pushes, no deletions, signed commits, and
green checks before a merge. `.github/workflows/ci.yml` is the single
definition of green — rustfmt, clippy, `cargo test --workspace`, the
service end-to-end rig and the headless GUI smoke check. The coverage and
audit jobs are informational and never block.

Releases cut from `main` only, and only when the push touches the app
itself (see "Building" above for the path list). Each release mints its
own version: the workspace manifest bumped past the latest tag,
patch-only unless minor or major was raised in the root `Cargo.toml` by
hand. The tag is pushed after the builds pass, so a failed run leaves
nothing behind — and nothing is ever committed back: version bumps live
only in the built artifacts.

## Licence

GPL-3.0-or-later. See `COPYING`.
