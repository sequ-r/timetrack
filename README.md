# TimeTrack

A time tracker for GNOME. You **enter** time — a quick tap for the common case,
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
  timetrack (GUI, flatpak)        timetrack (CLI, static binary)
  gpui 0.2.2, 3 tabs               ratatui TUI + one-shot commands
        |                                  |
        +--------- D-Bus: org.sequ.timetrack.Entries ----+
                           |
              timetrack-service  (D-Bus activated)
              owns the store; does all aggregation
                           |
                  ~/.local/share/timetrack/store.json
```

| Crate | Role |
|---|---|
| `timetrack-core` | Model, rules, aggregation, atomic JSON storage. No GUI, IPC, async, **or clock**. |
| `timetrack-proto` | The D-Bus contract and a client. Both front ends depend on this. |
| `timetrack-service` | D-Bus server. Owns the store, resolves the clock and timezone, persists on every change. |
| `timetrack-cli` | Terminal client: a ratatui TUI plus one-shot subcommands. |
| `timetrack-gui` | Desktop client, built on gpui (upstream, from crates.io). Ships as the flatpak. |

The core takes an explicit timestamp and never reads the clock, so the rules are
identical everywhere and testable without any I/O. Aggregation is implemented in
the core but *applied* by the service, so the GUI and the CLI cannot disagree
about where a week starts.

## What works today

| | |
|---|---|
| Four ways to add time | explicit interval, duration ending now, duration ending earlier, quick-add (5/15/30/60) |
| Three ways to take it back | `shorten` moves an endpoint, `undo` removes a quick-add, `delete` is explicit |
| Split and merge | on the core and the service; not yet reachable from the GUI |
| Totals | this ISO week, this month, all time, per project |
| Overlap | counted twice, by design; a day over 24h is flagged, never rejected |
| Projects | create and archive; archiving keeps the history in totals |
| Persistence | atomic writes; survives restart; corrupt or newer stores are refused, never silently reset |

**Not built yet.** Split and merge are not wired to the GUI. The
GUI's "New project" button invents a numbered default name, because there is no
text field yet. Merging overlapping entries shrinks the total by the overlap,
which is deliberate (§8) but not yet explained in a confirmation dialog, because
there is no confirmation dialog yet. The GUI Export tab writes
`~/timetrack-export-<week|month|all>.csv` with no file picker yet (portal save
dialog is the follow-up).

## Building

Requires Rust 1.85 or newer (the crates are edition 2024).

```sh
cargo build --release
cargo test --workspace        # 136 unit tests
bash scripts/e2e.sh           # 60 end-to-end checks against a real service
bash scripts/gui-smoke.sh     # 10 checks that the GUI paints, clicks, and reads the keyboard
```

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

Start the service once, then either client:

```sh
timetrack-service &
timetrack                      # TUI
timetrack quick -m 15          # or a one-shot command
timetrack status
```

The TUI: `1`-`4` quick-add, `a` 30 minutes, `u` undo, `d` delete, `j`/`k` move,
`h`/`l` change project, `q` quit. Projects are created from the command line
(`timetrack project "Work"`), since the TUI has no text field yet.

### The service has to be installed once

The GUI is a flatpak and cannot run host binaries, so it cannot start the service
itself. Install the service and a D-Bus activation file on the host; after that
the session bus starts it on demand and the GUI never has to:

```sh
install -Dm755 target/release/timetrack-service ~/.local/bin/timetrack-service
mkdir -p ~/.local/share/dbus-1/services
sed 's|/usr/bin/timetrack-service|'"$HOME"'/.local/bin/timetrack-service|' \
  data/org.sequ.timetrack.service.in \
  > ~/.local/share/dbus-1/services/org.sequ.timetrack.service
```

**After upgrading, reinstall the service.** The interface was renamed from
`org.sequ.timetrack.Timer` to `org.sequ.timetrack.Entries` when the timer was
cut. A stale service binary keeps answering to the old name and the GUI reports
`UnknownInterface`. It now says so explicitly rather than claiming the service is
merely absent, because the two need different fixes:

> The GUI and the service are different versions. Reinstall whichever one is
> older so both speak `org.sequ.timetrack.Entries`.

To run it at login instead:

```sh
mkdir -p ~/.config/systemd/user
cp data/org.sequ.timetrack.service ~/.config/systemd/user/
systemctl --user enable --now timetrack.service
```

Note that `Exec=` in a D-Bus service file is **not** run through a shell, so it
must be an absolute path to a real executable -- `sh -c ...` does not work.

## The terminal client

`timetrack` is not a flatpak — it is an ordinary binary:

```sh
cargo install --git https://github.com/sequ-r/timetrack.git timetrack-cli
```

Release tags also publish statically linked musl builds for x86_64 and
aarch64, so a single artifact runs on any Linux regardless of glibc version:

```sh
curl -LO https://github.com/sequ-r/timetrack/releases/latest/download/timetrack-x86_64.tar.gz
tar xzf timetrack-x86_64.tar.gz && install -m755 timetrack-x86_64 ~/.local/bin/timetrack
```

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
```

Timestamps accept `-90` for "90 minutes ago" or `09:30` for that time today.
Durations accept `90m`, `1h30m` or `45s`; a bare number means minutes.

The asymmetry is deliberate: `shorten` can never delete anything, and `undo`
**refuses** an entry it did not create, so it cannot lose hand-entered time by
accident.

Note for packagers: **do not set `CARGO_TARGET_*_MUSL_LINKER=musl-gcc`**. Some
distributions' `musl-gcc` (GCC 16.x on Arch as of 2026-09) produces binaries
that segfault at startup on anything that hashes — `std`'s `HashMap`, which
zbus' D-Bus address parser needs. Rust's default linker for musl targets is
correct; the override is the bug. See `.cargo/config.toml`.

## Flatpak

`org.sequ.timetrack.json` builds against **GNOME 50**; `org.sequ.timetrack.beta.json`
targets **GNOME 51**. Both build the same tree.

```sh
./build-flatpak.sh            # GNOME 50 (stable)
./build-flatpak.sh beta       # GNOME 51
```

Or by hand:

```sh
rm -rf .flatpak-builder build   # this step matters, see below
flatpak-builder --user --install --force-clean build org.sequ.timetrack.json
```

**Clear `.flatpak-builder` after pulling.** It mirrors the git source into
`.flatpak-builder/git/` and reuses that clone, so it can lag behind
`origin/main` — the build then fails on a file that exists in your checkout,
which looks like the manifest is wrong when it is the cache. A stale cache is
also how a build can report `Success!` while shipping a binary from an older
commit. The helper script does the clearing for you.

A note on the toolchain: the manifest does **not** use
`org.freedesktop.Sdk.Extension.rust-stable`, because Flathub publishes that
extension only for the freedesktop `25.08` and `26.08` branches — there is no
`//50` or `//51`, and `flatpak-builder` appends the runtime branch, so asking
for `//26.08` resolves to `//26.08/50`, which does not exist. Instead the
build vendors a pinned rustup (1.98.0, `--profile minimal`). This costs a
toolchain download per build; the version is pinned so builds stay
reproducible.

## Known issues

**The GNOME 51 beta manifest is unbuilt.** `org.sequ.timetrack.beta.json`
is identical to the GNOME 50 one apart from `runtime-version`, and the GNOME
50 build succeeds, but the 51 build has not been run.

**The timezone offset is resolved once, at startup, and only for fixed-offset
`TZ` values.** `UTC+2` and `UTC-05:30` work; a named zone such as
`Europe/Rome` falls back to UTC. So a machine observing DST can bucket an
entry into the wrong local day within a few hours of a transition, which
matters most at a week boundary. This is the likeliest remaining source of
wrong numbers, and a real tz-database lookup per instant is the fix.

## Licence

GPL-3.0-or-later. See `COPYING`.
