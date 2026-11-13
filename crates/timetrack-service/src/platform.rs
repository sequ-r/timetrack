/* service/platform.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Per-OS locations (§16): the one small place that knows where a
//! desktop OS wants an app's data. Pure over explicit inputs, like the
//! socket path derivation, so every OS's rule is testable on any host.

use std::ffi::OsStr;
use std::path::PathBuf;

/// The conventional store location, still spelled `store.json` (the
/// service argument's meaning since v1; the SQLite database is derived
/// beside it as `store.db`, and this path is where a legacy store is
/// imported from).
pub fn default_store_path() -> PathBuf {
    data_dir().join("store.json")
}

/// This OS's per-user data directory for TimeTrack.
pub fn data_dir() -> PathBuf {
    data_dir_from(
        std::env::consts::OS,
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
    )
}

fn non_empty(value: Option<&OsStr>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn data_dir_from(
    target_os: &str,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    app_data: Option<&OsStr>,
) -> PathBuf {
    let base = match target_os {
        "windows" => non_empty(local_app_data)
            .or_else(|| non_empty(app_data))
            .or_else(|| non_empty(home).map(|h| h.join("AppData").join("Local"))),
        "macos" => non_empty(home).map(|h| h.join("Library").join("Application Support")),
        _ => non_empty(xdg_data_home)
            .or_else(|| non_empty(home).map(|h| h.join(".local").join("share"))),
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("timetrack")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn linux_matches_the_v1_xdg_location() {
        // The same bytes JsonStore::default_path produced, so an
        // existing Linux install finds its store after the port.
        assert_eq!(
            data_dir_from("linux", os("/data"), os("/home/u"), None, None),
            PathBuf::from("/data/timetrack")
        );
        assert_eq!(
            data_dir_from("linux", None, os("/home/u"), None, None),
            PathBuf::from("/home/u/.local/share/timetrack")
        );
        assert_eq!(
            data_dir_from("linux", os(""), os("/home/u"), None, None),
            PathBuf::from("/home/u/.local/share/timetrack"),
            "an empty XDG_DATA_HOME counts as unset"
        );
    }

    #[test]
    fn macos_uses_application_support() {
        assert_eq!(
            data_dir_from("macos", None, os("/Users/u"), None, None),
            PathBuf::from("/Users/u/Library/Application Support/timetrack")
        );
    }

    #[test]
    fn windows_prefers_local_app_data() {
        assert_eq!(
            data_dir_from(
                "windows",
                None,
                os("C:\\Users\\u"),
                os("C:\\Users\\u\\AppData\\Local"),
                os("C:\\Users\\u\\AppData\\Roaming")
            ),
            PathBuf::from("C:\\Users\\u\\AppData\\Local").join("timetrack")
        );
        assert_eq!(
            data_dir_from(
                "windows",
                None,
                os("C:\\Users\\u"),
                None,
                os("C:\\Users\\u\\AppData\\Roaming")
            ),
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming").join("timetrack")
        );
        assert_eq!(
            data_dir_from("windows", None, os("C:\\Users\\u"), None, None),
            PathBuf::from("C:\\Users\\u")
                .join("AppData")
                .join("Local")
                .join("timetrack")
        );
    }

    #[test]
    fn no_home_anywhere_falls_back_to_the_working_directory() {
        assert_eq!(
            data_dir_from("linux", None, None, None, None),
            PathBuf::from("./timetrack")
        );
    }
}
