# The D-Bus service interfaces are retired (#129)

Status: accepted; retires both D-Bus interfaces from [0012](0012-typed-dbus-interface.md) after the dual-served socket transition of [0013](0013-multiplatform-local-socket-ipc.md).

The service now speaks only its local socket. Both D-Bus interfaces — the original JSON-in-string `org.sequ.timetrack.Entries` and the typed `Entries2` — are removed from the service and from `timetrack-proto`, along with the `WireError`/typed-twin conversions and the client's bus transport. `timetrack-service` and `timetrack-cli` no longer depend on zbus at all. The wire contract did not change shape in the move: the socket carries the same view structs and the same typed `ServiceError` refusals, so this is a transport retirement, not a model change. The e2e rig got its snapshot oracle from a new `timetrack snapshot` JSON command instead of `gdbus`, and no longer starts a session bus.

What deliberately stays on D-Bus, because those protocols *are* D-Bus: the tray's StatusNotifier item and dbusmenu, and `desktop_notify`'s `org.freedesktop.Notifications` call. Deleting `timetrack`'s own bus name was the goal; the desktop's buses were never ours to retire.

The cost is the one [0012](0012-typed-dbus-interface.md) accepted in advance: a client or service from before the socket transition can no longer talk to this build at all. With one AppImage carrying every client and the service together, that pairing only exists across an upgrade boundary the user crosses by replacing the one file.
