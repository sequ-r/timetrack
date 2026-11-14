# Multiplatform desktop on local-socket IPC, not D-Bus

Status: accepted; supersedes the transport half of [0012](0012-typed-dbus-interface.md) and scopes [0008](0008-appimage-is-the-install.md) to Linux.

TimeTrack is multiplatform from v2 (REQUIREMENTS §16): the same core → service → GUI/CLI stack runs on Linux, Windows, and macOS, local-first and sync-ready (UUIDs and writer metadata, §20) but with no account or server yet. The service's transport moves from the D-Bus session bus to local sockets — Unix sockets on Linux/macOS, named pipes on Windows — keeping 0012's typed contract, named errors, and `Client` API, with the `Changed` push as a subscription on the connection. Socket ownership replaces bus-name ownership as the single-instance mechanism, any client auto-spawns the sibling service binary when the socket does not answer (replacing the AppRun shell launcher), and D-Bus is dual-served beside sockets for one Linux release, then dropped together with the legacy JSON interface (#129). The AppImage stays the Linux install; Windows and macOS get their own packaging, so 0008's "one file is the install" now holds per-OS, not globally.

The hero is part of this decision, not decoration: every platform's main page opens with the week total in big bold characters, nothing above it (§11), and no port may demote it.

As built (step 4): std Unix sockets with length-prefixed JSON frames (`timetrack-proto::socket`), one request per connection plus a subscription connection for `Changed`; clients probe the socket first and fall back to the bus, spawn the sibling service binary once when nothing answers, and the service serves socket-only when no session bus exists. The `interprocess` crate was not adopted: its async surface is tokio-gated (banned in this workspace) and std covers Unix with no new dependency; Windows named pipes land behind the same module in step 5.

## Considered Options

- **Localhost HTTP.** Rejected: a TCP listener is a network surface (firewall prompts, port squatting, other local users) for what is strictly same-user IPC; sockets/pipes carry the OS's own per-user permissions.
- **Keep D-Bus on Linux, different IPC per other OS.** Rejected: three transports to keep in step forever, when one socket contract serves all three and the typed API above it does not change.
- **Mobile in v2.** Deferred: Android/iOS reuse this core and service API later; shaping v2 around them now would force the sync/account design §20 deliberately only prepares for.
