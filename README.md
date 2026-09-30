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

`timetrack` is not a flatpak — it is an ordinary binary you can install
directly:

```sh
cargo install --git https://github.com/sequ-r/timetrack.git timetrack-cli
```

Prebuilt release binaries are **not** published yet. A statically linked musl
build was attempted and segfaults on startup (12/12 runs), so it was removed
rather than shipped; a static build of the ratatui + zbus stack is not yet
working. Install with cargo, or build from source, until that is fixed.

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

**The GUI compiles and links, but panics on startup.** zbus' `tokio` backend
performs the bus connect inside zbus' *own* executor thread, which has no tokio
context, and calls `tokio::task::spawn_blocking` there — panicking with "there
is no reactor running, must be called from the context of a Tokio 1.x runtime".
gpui drives its executor on threads we do not control, and Cargo unifies zbus'
features across the workspace, so the GUI cannot simply drop the `tokio`
backend that the service and CLI rely on. Everything bus-related is already
confined to a dedicated thread with its own runtime in `crates/timetrack-gui/src/app.rs`;
what remains is getting that runtime's context visible to zbus' internal
thread. Until that is resolved the GUI is buildable but not runnable, and the
flatpak is not yet publishable.

**No prebuilt binaries are published.** A statically linked musl build of the
CLI compiles and links with no `NEEDED` entries, but segfaults on startup
(12/12 runs). Install with `cargo install` instead.

## Licence

GPL-3.0-or-later. See `COPYING`.
