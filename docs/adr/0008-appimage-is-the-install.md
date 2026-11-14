# The AppImage is the install

Status: scoped to Linux by [0013](0013-multiplatform-local-socket-ipc.md) — still the whole Linux install story; Windows and macOS get their own packaging under the same per-OS rule.

The GUI, the service and the CLI ship as one file: `build-appimage.sh`
bundles `timetrack-gui`, `timetrack-service` and `timetrack` into
`TimeTrack-<arch>.AppImage`, and `release.yml` publishes nothing else.
There are no musl CLI tarballs any more, and no host install of the
service -- no D-Bus activation file, no systemd unit.

AppRun dispatches on how the image was invoked: the bare image starts the
bundled service in the background and then the GUI; a `timetrack` symlink
(or `cli` as the first argument) runs the bundled CLI instead, starting
the service the same way. Both clients talk to whichever service owns the
bus name, so a second copy's service exits harmlessly.

The point is uninstall: the image writes nothing to the host, so deleting
the file (plus the opt-in `timetrack` symlink) is the whole uninstall.
An installer that wrote host files pointing at the image would leave them
dangling the moment the image is deleted -- D-Bus activation into nowhere,
a failing user unit, a dead symlink. Nothing here can dangle because
nothing is written.

Host `cargo build` binaries remain as a development fallback; they are not
an install and the README says so.
