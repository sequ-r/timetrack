# TimeTrack

A time tracker for GNOME. Start a timer, stop it, and keep a history you can
review — from a desktop window or the terminal.

The running timer lives in a **service**, not in either front end, so closing
the GUI or the terminal never stops what you are timing. Both clients are
clients, and both can be open at once.

## Architecture

```
  timetrack (GUI, flatpak)        timetrack (CLI, static binary)
  gpui-ce                         ratatui + one-shot commands
        |                                  |
        +--------- D-Bus: org.sequ.timetrack ----+
                           |
              timetrack-service  (systemd --user)
              owns the running timer
                           |
                  ~/.local/share/timetrack/store.json
```

| Crate | Role |
|---|---|
| `timetrack-core` | Domain model, timer state machine, atomic JSON storage. No GUI, IPC or async. |
| `timetrack-proto` | The D-Bus contract and a client. Both front ends depend on this. |
| `timetrack-service` | D-Bus server. Owns the running timer; persists on every change. |
| `timetrack-cli` | Terminal client: a ratatui TUI plus `start`/`stop`/`status`/`list`. |
| `timetrack-gui` | Desktop client, built on gpui-ce. Ships as the flatpak. |

The state machine takes `&mut Store` and an explicit timestamp and never reads
the clock itself, so the rules are identical everywhere and testable without
any I/O.

## Building

Requires Rust 1.85 or newer (the crates are edition 2024).

```sh
cargo build --release
cargo test --workspace
```

On Linux the GUI additionally needs the system libraries gpui links against:

```sh
# Debian/Ubuntu
apt install libxkbcommon-dev libxkbcommon-x11-dev libfreetype-dev libfontconfig-dev
```

## Running

The service owns the running timer; the clients are clients. Start it once and
leave it:

```sh
timetrack-service &
```

Then either client:

```sh
timetrack                    # TUI
timetrack start "writing"    # or one-shot
timetrack status
```

The TUI: `space` start/stop, `c` cancel, `d` delete, `j`/`k` move, `q` quit.

### The service has to be installed once

The GUI is a flatpak and cannot run host binaries, so it cannot start the
service itself. Install the service and a D-Bus activation file on the host;
after that the session bus starts it on demand and the GUI never has to:

```sh
install -Dm755 timetrack-service ~/.local/bin/timetrack-service
mkdir -p ~/.local/share/dbus-1/services
sed 's|/usr/bin/timetrack-service|$HOME/.local/bin/timetrack-service|' \
  data/org.sequ.timetrack.service.in \
  > ~/.local/share/dbus-1/services/org.sequ.timetrack.service
```

Without that, the window opens but reports that the service is not running. The
GUI also offers a **Start the service** button, which works once the activation
file is in place.

To have it running at login instead:

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
which looks like the manifest is wrong when it is the cache. The helper script
does this for you.

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

## Licence

GPL-3.0-or-later. See `COPYING`.
