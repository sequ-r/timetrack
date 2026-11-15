# macOS bundle and disk image (#159)

One conventional install, `TimeTrack-<version>-macos-<arch>.dmg`: a
drag-to-Applications disk image holding `TimeTrack.app`, which bundles the
GUI, the CLI (`timetrack`), the service and the menu-bar tray. The Linux
AppImage path is untouched — this directory, the `macos-dmg` job in
`release.yml`, and these notes are the whole slice, extending the #158
release additions without rewriting them.

## Why a DMG

| Candidate | Verdict |
|---|---|
| DMG (drag to /Applications) | Chosen: the conventional GUI install — no installer code to write or review, uninstall is Trash, and a signed DMG notarizes and staples as one artifact. Matches ADR 0013's per-OS "one file is the install" the same way the AppImage does. |
| Zip of the .app | Rejected: no drag-install affordance, no volume affordance at all, and a quarantined zip extracts into a quarantined app with the worse first-launch UX. Same bundle inside, poorer install. |
| pkg installer | Rejected: runs an installer with receipt bookkeeping for what is a per-user drag install, implies system-wide placement, and needs a custom uninstaller to leave nothing dangling. Nothing here needs install-time scripts. |
| Homebrew cask | Not packaging: a fine later pointer at the DMG, but the DMG below is what it would point at. |

Single-arch v1: the build names the runner's own arch (`uname -m`,
`arm64` on `macos-latest`) into the artifact. Universal2 via `lipo`
waits for a proven Intel half — there is no Intel CI runner proving the
build today, so v1 ships Apple silicon only and says so. An Intel Mac
needs a future universal build, not this DMG.

`LSMinimumSystemVersion` is 13.0 Ventura: the oldest release this slice
was written against. Older systems are untested, so the bundle refuses
them plainly instead of failing oddly; lower only with an older-host run.

## Bundle layout

```
TimeTrack.app/
  Contents/
    Info.plist            (this directory's template, version-stamped)
    MacOS/
      timetrack-gui       (CFBundleExecutable: opening the app opens this)
      timetrack-service   (sibling: the auto-spawn target, see below)
      timetrack           (CLI; symlink it onto PATH, see below)
      timetrack-tray      (menu-bar extra)
    Resources/
      TimeTrack.icns      (generated at build from data/icons, see below)
```

All four binaries sit beside each other in `MacOS/` — not split into a
`Helpers/` subdir — because the sibling-spawn constraint requires it:
`try_spawn_service` (`crates/timetrack-proto/src/socket.rs`) resolves the
service as `current_exe.parent() / "timetrack-service"`, and the GUI's
executable directory *is* `Contents/MacOS`, so the service must live
there or first launch finds no service. The tray's Open row resolves the
GUI the same sibling way (`crates/timetrack-tray/src/platform.rs`), so
the tray sits there too. The conventional `Helpers/` split would break
both lookups without Rust changes, which packaging must not require.

Bundle ID is `org.sequ.timetrack`, matching the Linux desktop file and
the tray's single-instance name: one reverse-DNS root everywhere.

The `.icns` is build dirt, generated on the macOS runner by
`sips` + `iconutil` from the shipped 256px
`data/icons/hicolor/256x256/apps/org.sequ.timetrack.png`. The 512/1024
entries upscale from that 256px master — this slice's documented v1
limitation (a 1024px master is future work, not a blocker: the icon reads
fine, it is just not pixel-perfect on Retina).

## Behavior contract

- **Install**: open the DMG, drag `TimeTrack.app` to `/Applications`
  (or `~/Applications` for one user only). No scripts run, no UAC-style
  prompt, nothing outside the bundle is written.
- **First launch**: double-click opens the GUI, which auto-spawns the
  sibling service when the socket does not answer (REQUIREMENTS §16) —
  no daemon to register, no LaunchAgent installed. A signed + notarized
  build opens straight through Gatekeeper; see the unsigned fallback
  below for the other case.
- **CLI access**: a DMG runs no scripts, so nothing is put on `PATH`
  for you. Symlink it yourself, mirroring the Linux install:
  `ln -s /Applications/TimeTrack.app/Contents/MacOS/timetrack ~/.local/bin/timetrack`.
- **Menu-bar extra**: run
  `/Applications/TimeTrack.app/Contents/MacOS/timetrack-tray` once (or
  keep it running); Quit stops the tray only, never the service. There
  is no autostart registration by design — add the tray binary to Login
  Items yourself (System Settings → General → Login Items) if you want
  it at logon, and it is yours to remove again.
- **Upgrade**: open the new DMG and drag-replace the app. The bundle is
  replaced whole; user data lives outside it (below), so nothing migrates.
- **Uninstall**: quit the GUI and the tray (its Quit row), stop the
  service (`killall timetrack-service` — it intentionally outlives the
  window, as on every OS), remove any Login Item you added, and drag the
  app to Trash. No receipts, no LaunchAgents, no registry: nothing of the
  install can dangle.
- **Data always survives**: `~/Library/Application Support/timetrack`
  (the store, `service/src/platform.rs`) is never created, touched, or
  deleted by any of install / upgrade / uninstall — the DMG writes only
  the bundle. Uninstalling and reinstalling keeps every entry.

## Building

On macOS with Xcode command line tools (CI does this for you):

```sh
./packaging/macos/build-bundle.sh --version 0.2.0
# → dist/TimeTrack-0.2.0-macos-arm64.dmg (+ sha256 printed)
```

`--version` defaults to the workspace `Cargo.toml` version. The script
builds all four release binaries unless `--skip-build` is given, stages
the bundle under `packaging/macos/build/` (git-ignored build dirt, like
the Windows `bin/`), stamps, signs, images, notarizes and staples.

## Signing, notarization, and the unsigned fallback

Both halves are done by the build script from maintainer-supplied repo
secrets, never baked into a config file:

1. Maintainer exports the Developer ID certificate as P12 and stores it
   in `APPLE_CERT_P12_BASE64` (base64 of the `.p12`) and
   `APPLE_CERT_PASSWORD` (may be empty), plus `APPLE_DEVELOPER_ID`
   (e.g. `Developer ID Application: Name (TEAMID)`).
2. The script imports the P12 into a throwaway keychain (the login
   keychain is never touched), signs each binary inside-out with the
   Hardened Runtime (`--options runtime`, timestamped, explicit
   [TimeTrack.entitlements](TimeTrack.entitlements)) and the bundle and
   DMG after them, then deletes the keychain. The entitlements audit
   lives in that file: Hardened Runtime on, no sandbox, no capability
   entitlements required.
3. Notarization additionally needs `APPLE_ID`, `APPLE_APP_PASSWORD`
   and `APPLE_TEAM_ID`: `notarytool submit --wait`, then `stapler
   staple` + `validate` on the DMG.
4. `release.yml` passes those secrets through; the stapled DMG ships in
   the release next to the AppImage and the Windows installer with one
   shared `SHA256SUMS`.

**Unsigned fallback (explicit):** with no `APPLE_CERT_P12_BASE64` the
build still succeeds and prints an `UNSIGNED build` warning. Such a DMG
installs and runs, but Gatekeeper refuses the quarantined app until the
user right-click-Opens it once (Finder → right click → Open). A release
cut without the secrets **must say so in its release notes** ("macOS
disk image is unsigned in this release; right-click-Open on first
launch") — that sentence, not silence, is the accepted limitation from
#159. Signed-but-unnotarized (certificate without the API secrets) is
the middle state: Gatekeeper still warns on first launch, and the notes
must say that instead.

## Verification status

Verifiable from Linux (done in this slice): `Info.plist` and the
entitlements parse and carry the asserted keys, the build script passes
`bash -n` plus its argument probes, the icon source exists at the path
the script reads, `release.yml` still parses and publishes the new
artifact with checksums.

Not verifiable here — stays open until a macOS run: `cargo build` on the
runner, `sips`/`iconutil`, `codesign`, `notarytool`/`stapler`, `hdiutil`,
and the install → launch → track → restart → upgrade → uninstall
walkthrough on a clean account (#159 acceptance). `release.yml` mounts
the DMG and runs the bundled CLI's `--version` on the build runner as
the first half of that; the clean-account half is manual.
