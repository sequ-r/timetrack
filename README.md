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

Start the service, then whichever client you want:

```sh
timetrack-service &          # or via the systemd user unit, see below
timetrack                    # TUI
timetrack start "writing"    # or one-shot
timetrack status
```

The TUI: `space` start/stop, `c` cancel, `d` delete, `j`/`k` move, `q` quit.

To run the service at login:

```sh
mkdir -p ~/.config/systemd/user
cp data/org.sequ.timetrack.service ~/.config/systemd/user/
systemctl --user enable --now timetrack.service
```

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
flatpak-builder --user --install --force-clean build org.sequ.timetrack.json
```

A note on the toolchain: the manifest does **not** use
`org.freedesktop.Sdk.Extension.rust-stable`, because Flathub publishes that
extension only for the freedesktop `25.08` and `26.08` branches — there is no
`//50` or `//51`, and `flatpak-builder` appends the runtime branch, so asking
for `//26.08` resolves to `//26.08/50`, which does not exist. Instead the
build vendors a pinned rustup (1.98.0, `--profile minimal`). This costs a
toolchain download per build; the version is pinned so builds stay
reproducible.

## Known issues

**The GUI has never displayed a window.** It builds, links, and starts without
panicking, and under `Xvfb` it maps `libX11`, `libGLX` and `libvulkan` and runs
30 threads including its zbus connection thread — so the process is healthy. But
no window ever appears, and Xvfb cannot prove that it would.

`gpui_linux::current_platform` picks a backend from `gpui::guess_compositor()`,
which only looks at `WAYLAND_DISPLAY` and `DISPLAY`. With `DISPLAY=:99` it does
select X11 — yet the process ends up holding 5 DRM fds and *zero* X11 socket
fds, and rendering falls back to Vulkan, which Xvfb has no presentation path
for. So Xvfb exercises process startup and the gpui event loop, not rendering.

Testing rendering needs a real display: run it inside a nested Wayland
compositor (sway, cage or weston), or on a normal session of this machine.

## Licence

GPL-3.0-or-later. See `COPYING`.
