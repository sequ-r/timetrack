# Windows installer (Inno Setup, #158)

One conventional installer, `TimeTrack-Setup-<version>.exe`, bundling the
GUI, the CLI (`timetrack.exe`), the service and the tray. The Linux AppImage
path is untouched — this directory, the `windows-installer` job in
`release.yml`, and these notes are the whole slice.

## Why Inno Setup

| Candidate | Verdict |
|---|---|
| Inno Setup 7 | Chosen: one declarative `.iss` script, a CLI compiler (`ISCC.exe`) that runs headless on `windows-latest`, free (BSD-like), handles Start Menu / PATH / Run-key / uninstaller generation without custom code, and upgrades by stable `AppId` out of the box. |
| WiX | Rejected: far more verbose (dozens of XML elements for the same install), slower to review, and the v4 .NET-based toolchain is heavier in CI for no behavior we need. |
| NSIS | Rejected: imperative script with weaker defaults (uninstaller and upgrade semantics are hand-rolled); fine tool, but more code to maintain for the same result. |
| cargo-packager / cargo-wix | Rejected: ties packaging to the Rust toolchain and a third-party crate's template; an `.iss` is reviewable by anyone and outlives our dependency tree. |

`release.yml` pins `innosetup-7.1.1-x64.exe` from the vendor's GitHub
releases (`jrsoftware/issrc`, tag `is-7_1_1`) and verifies its SHA256 before
installing — the same pin-plus-checksum rule `build-appimage.sh` uses.
Targets: Windows 10 and 11, x64 (`MinVersion=10.0`,
`ArchitecturesAllowed=x64compatible` in [timetrack.iss](timetrack.iss)).

## Behavior contract

- **Per-user install, no UAC prompt** (`PrivilegesRequired=lowest`) into
  `%LOCALAPPDATA%\Programs\TimeTrack`. An admin can still pick all-users in
  the dialog.
- **Start Menu** entry for the GUI; optional desktop icon (unchecked by
  default).
- **PATH**: the install dir is appended to the *user* `Path` when the
  (default-on) task is selected, so `timetrack` works in a new terminal.
  Uninstall — or unchecking the task on a reinstall — removes exactly that
  entry and nothing else ([Code] in the script).
- **Autostart**: the (default-on) task writes
  `HKCU\...\CurrentVersion\Run\TimeTrack` pointing at `timetrack-tray.exe`.
  There is no Windows-service registration by design: any client auto-spawns
  the sibling `timetrack-service.exe` when the named pipe does not answer
  (REQUIREMENTS §16), so install registers nothing else.
- **Upgrade**: same `AppId`, so running a newer installer upgrades in place
  (running apps are offered close-first via `CloseApplications`). Task
  choices persist across upgrades (`checkedonce`).
- **Repair**: re-running the installer overwrites every file (`ignoreversion`).
- **Uninstall** (Add/Remove Programs or `unins000.exe`) removes the program
  dir, shortcuts, the Run value, and the PATH entry.
- **Data always survives**: `%LOCALAPPDATA%\timetrack` (the store) is never
  created, touched, or deleted by any of install / upgrade / repair /
  uninstall — the installer writes only under its own `{app}` plus the two
  registry values above. Uninstalling and reinstalling keeps every entry.

## Building

On Windows with Inno Setup 7 installed (CI does this for you):

```powershell
.\packaging\windows\build-installer.ps1 -Version 0.2.0
# → dist\TimeTrack-Setup-0.2.0.exe (+ sha256 printed)
```

`-Version` defaults to the workspace `Cargo.toml` version. The script builds
all four release binaries unless `-SkipBuild` is given, stages them into
`packaging\windows\bin\` (git-ignored build dirt, like `Output\`), and
compiles [timetrack.iss](timetrack.iss) with that version stamped in.

## Signing and the unsigned fallback

Authenticode signing is done by the build script, not the `.iss`, so the
secrets never pass through an Inno define:

1. Maintainer exports the code-signing certificate as PFX and stores it in
   the repo secrets `WINDOWS_PFX_BASE64` (base64 of the `.pfx`) and
   `WINDOWS_PFX_PASSWORD` (may be empty).
2. `build-installer.ps1` signs each staged `.exe` *before* compiling, then
   signs the finished installer, timestamping both via
   `http://timestamp.digicert.com` (RFC 3161, SHA-256).
3. `release.yml` passes those secrets through; the signed installer ships in
   the release next to the AppImage with one shared `SHA256SUMS`.

**Unsigned fallback (explicit):** with no `WINDOWS_PFX_BASE64` the build
still succeeds and prints an `UNSIGNED build` warning. Such an installer
works, but Windows SmartScreen shows an "unknown publisher" prompt. A
release cut without the secrets **must say so in its release notes**
("Windows installer is unsigned in this release") — that sentence, not
silence, is the accepted limitation from #158.

## Verification status

Verifiable from Linux (done in this slice): `.iss` structure (sections,
task/registry wiring, GUID shape) by inspection script, `timetrack.ico`
generated from the shipped 256px app icon and listed with `icotool`,
`release.yml` still parses and publishes the new artifact with checksums.

Not verifiable here — stays open until a Windows run: `ISCC` compile,
PowerShell execution incl. signing, and the install → launch → track →
restart → upgrade → uninstall walkthrough on a clean account (#158
acceptance). `release.yml` runs the installer `/VERYSILENT` plus an
uninstall on the build runner as the first half of that; the clean-VM half
is manual.
