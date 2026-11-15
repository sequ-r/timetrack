/* tray/platform.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Per-OS GUI launch commands: the one small place that knows how the
//! tray's Open row starts the app on each desktop OS.
//!
//! Linux parses the `Exec` line of the installed desktop entry, so the
//! tray and the launcher agree. Other targets return the sibling GUI
//! binary beside the tray (like the service auto-spawn); the full
//! per-OS launchers land with the Windows/macOS backends (#155/#156).
//! Pure over explicit inputs where possible, so every OS's rule stays
//! testable on any host.

/// The command the Open row runs, as argv with argv[0] first. Never empty:
/// the dispatcher's empty branch (the "no Exec line" log) then only fires
/// on Linux, where the wording names the desktop entry.
pub fn open_gui_command() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        gui_argv(DESKTOP_ENTRY)
    }
    #[cfg(not(target_os = "linux"))]
    {
        sibling_gui_command()
    }
}

/// The desktop entry whose `Exec` line the Open row runs
/// (`data/org.sequ.timetrack.desktop:5`). Baked at compile time so the tray
/// and the installed launcher always agree on how the app starts.
#[cfg(target_os = "linux")]
pub const DESKTOP_ENTRY: &str = include_str!("../../../data/org.sequ.timetrack.desktop");

/// Command the Open row runs on Linux: the desktop entry's `Exec` line,
/// first word as argv[0]. Field codes (`%U`, `%F`, ...) are desktop-file
/// placeholders for the launcher, not arguments, and are dropped. Pure
/// over the file text so tests pin it without spawning anything.
#[cfg(target_os = "linux")]
pub fn gui_argv(desktop: &str) -> Vec<String> {
    desktop
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))
        .map(|exec| {
            exec.split_whitespace()
                .filter(|arg| !arg.starts_with('%'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The sibling GUI binary beside the running tray executable, falling
/// back to the bare binary name when the layout is unknown. Stub for
/// #155/#156: those backends replace this with the real per-OS launch
/// (app bundle, Start-menu entry) while keeping this signature.
#[cfg(not(target_os = "linux"))]
fn sibling_gui_command() -> Vec<String> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let name = if cfg!(windows) {
            "timetrack-gui.exe"
        } else {
            "timetrack-gui"
        };
        return vec![dir.join(name).to_string_lossy().into_owned()];
    }
    vec!["timetrack-gui".to_string()]
}

/// The sibling GUI command, pure over explicit inputs so the rule is
/// testable on any host (mirrors `service/src/platform.rs`).
#[cfg(all(test, not(target_os = "linux")))]
fn sibling_gui_command_from(current_exe: Option<&std::path::Path>) -> Vec<String> {
    if let Some(dir) = current_exe.and_then(|exe| exe.parent()) {
        let name = if cfg!(windows) {
            "timetrack-gui.exe"
        } else {
            "timetrack-gui"
        };
        return vec![dir.join(name).to_string_lossy().into_owned()];
    }
    vec!["timetrack-gui".to_string()]
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn sibling_command_is_never_empty() {
        // The dispatcher's empty branch must never fire off Linux: its
        // wording names the desktop entry.
        assert!(!open_gui_command().is_empty());
        assert!(!sibling_gui_command_from(None).is_empty());
    }

    #[test]
    fn sibling_command_names_the_gui_beside_the_tray() {
        let exe = std::path::Path::new("/opt/timetrack/timetrack-tray");
        let argv = sibling_gui_command_from(Some(exe));
        assert_eq!(argv.len(), 1);
        assert!(argv[0].ends_with("timetrack-gui"), "argv: {argv:?}");
    }
}
