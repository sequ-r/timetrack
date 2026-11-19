/* gui/app/export/save.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The export save pipeline: from a fetched CSV to bytes on disk.
//!
//! Asks the system save dialog where to write (ticket 27), writes through a
//! portal grant or falls back to the direct home-directory save, and reports
//! the outcome as one message. Runs off the service thread, so the save
//! dialog never stops that thread from serving.

use super::super::service::{ExportRequest, Msg, RangeExportRequest};
use super::super::status::{StatusKind, StatusMsg};
use std::sync::mpsc::Sender;
use timetrack_proto::Snapshot;

/// Ask where to put an export and write it there.
///
/// Runs off the service thread, so the save dialog never stops that thread
/// from serving: the window keeps painting and refreshing, and a quick-add
/// made while the dialog is open still lands in the store.
pub(crate) async fn save_export(tx: Sender<Msg>, csv: String, req: ExportRequest) {
    let ExportRequest {
        scope,
        path,
        suggested_name,
        period_id,
        ..
    } = req;
    save_fetched_csv(tx, csv, &suggested_name, &path, |csv, dest, via_portal| {
        export_write_msg(csv, dest, scope, &period_id, via_portal)
    })
    .await;
}

/// Ask where to put a custom-range export and write it there (#133).
///
/// The scoped save above stays untouched: the range carries no
/// `ExportScope`, so it names its own status parts instead of borrowing
/// the scope's.
pub(crate) async fn save_range_export(tx: Sender<Msg>, csv: String, req: RangeExportRequest) {
    let RangeExportRequest {
        path,
        suggested_name,
        period_id,
        ..
    } = req;
    save_fetched_csv(tx, csv, &suggested_name, &path, |csv, dest, via_portal| {
        export_range_write_msg(csv, dest, &period_id, via_portal)
    })
    .await;
}

/// The dialog plus fallback shared by every export save: the dialog opens
/// after the fetch, so a service refusal never prompts for a location.
/// Cancel is a quiet status that writes nothing; no portal falls back to
/// the direct home-directory write.
async fn save_fetched_csv(
    tx: Sender<Msg>,
    csv: String,
    suggested_name: &str,
    fallback_path: &str,
    make_msg: impl FnOnce(&str, &str, bool) -> Msg,
) {
    let outcome = portal_save_file(suggested_name).await;
    let via_portal = matches!(outcome, PortalOutcome::Granted(_));
    match resolve_export_destination(outcome, fallback_path) {
        Some(dest) => {
            let _ = tx.send(make_msg(&csv, &dest, via_portal));
        }
        None => {
            let _ = tx.send(export_cancel_msg());
        }
    }
}

/// The name prefilled in the system save dialog for a scope plus anchor
/// (ticket 27): the period-stamped default, so one week never silently
/// overwrites the last; all time ignores the anchor and keeps the plain name.
pub(crate) fn export_suggested_name(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    timetrack_core::default_export_filename(scope, anchor_ms, tz)
}

/// What the system save dialog answered (ticket 27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PortalOutcome {
    /// The user confirmed: write through this granted path, never the
    /// direct fallback.
    Granted(String),
    /// The user cancelled: a quiet no-op status, writes nothing, never an
    /// error (ticket 28).
    Cancelled,
    /// No portal answered (headless CI): the direct home-directory save runs
    /// on as the automatic fallback.
    Unavailable,
}

/// Where the fetched CSV goes: the granted path on confirm, nowhere on
/// cancel (`None` means the caller sends the quiet cancel status and writes
/// nothing), the home-directory fallback when no portal is available.
pub(crate) fn resolve_export_destination(outcome: PortalOutcome, fallback: &str) -> Option<String> {
    match outcome {
        PortalOutcome::Granted(granted) => Some(granted),
        PortalOutcome::Cancelled => None,
        PortalOutcome::Unavailable => Some(fallback.to_string()),
    }
}

/// Headless-test seam for the save dialog (tickets 27-28).
///
/// When `TIMETRACK_PORTAL_STUB` is `record:<file>`, the requested dialog
/// name is appended to `<file>` and the portal counts as unavailable, so
/// headless runs exercise the dialog wiring (which name is requested) while
/// the home-directory fallback keeps working. Anything else is not a stub.
///
/// Edge-mode switches for the ticket-28 pins, still with no live portal:
/// touching `<file>.cancel` makes the next stubbed save answer Cancelled
/// (quiet no-op, writes nothing), and `<file>.fail` grants a path whose
/// parent does not exist (local error naming path plus reason). Both still
/// log the requested name first, so the dialog wiring stays pinned.
///
/// `<file>.hold` makes the next stubbed save *wait* before answering, the
/// way a real dialog waits for a person: the outcome is decided first, so
/// removing the flag while the stub sleeps does not change it. That is what
/// pins that the window keeps serving while the dialog is open.
///
/// This whole seam is compiled in only with the `portal-stub` feature. A
/// released binary has no stub to arm, so the system save dialog cannot be
/// talked out of the way by an environment variable.
#[cfg(feature = "portal-stub")]
pub(crate) fn portal_stub_outcome(stub: &str, current_name: &str) -> Option<PortalOutcome> {
    let log = stub.strip_prefix("record:")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok()?;
    use std::io::Write;
    writeln!(file, "{current_name}").ok()?;
    let outcome = if std::path::Path::new(&format!("{log}.cancel")).exists() {
        PortalOutcome::Cancelled
    } else if std::path::Path::new(&format!("{log}.fail")).exists() {
        PortalOutcome::Granted(format!("/nonexistent-timetrack-gui/{current_name}"))
    } else {
        PortalOutcome::Unavailable
    };
    if std::path::Path::new(&format!("{log}.hold")).exists() {
        std::thread::sleep(std::time::Duration::from_secs(6));
    }
    Some(outcome)
}

/// The stubbed answer, if the stub is compiled in and armed.
#[cfg(feature = "portal-stub")]
pub(crate) fn stubbed_outcome(current_name: &str) -> Option<PortalOutcome> {
    let stub = std::env::var("TIMETRACK_PORTAL_STUB").ok()?;
    portal_stub_outcome(&stub, current_name)
}

/// The stubbed answer, if the stub is compiled in and armed.
#[cfg(not(feature = "portal-stub"))]
pub(crate) fn stubbed_outcome(_current_name: &str) -> Option<PortalOutcome> {
    None
}

/// Ask the system save dialog where to write an export (ticket 27).
///
/// The period-stamped `current_name` is prefilled so one week never silently
/// overwrites the last; the dialog's own overwrite question governs existing
/// files, so there is exactly one prompt. Cancel is quiet (`Cancelled`);
/// anything but a confirm or a cancel — no portal frontend, no session bus,
/// a native backend that panicked instead of answering — counts as
/// unavailable and the direct fallback runs.
pub(crate) async fn portal_save_file(current_name: &str) -> PortalOutcome {
    if let Some(outcome) = stubbed_outcome(current_name) {
        return outcome;
    }
    platform_portal_save_file(current_name).await
}

/// The native OS save panel on Windows and macOS (#154).
///
/// The same period-stamped `current_name` Linux prefills, the same title,
/// and the same contract: the OS owns the one overwrite prompt, cancel is
/// quiet, and a granted path is labelled as a chosen path downstream. The
/// blocking wait runs on the export's own thread off the service thread
/// (see the `Action::ExportCsv` arm), so snapshots and quick-adds keep
/// flowing while the dialog is open — rfd marshals to the main thread
/// itself where the OS needs it (macOS), so no main-thread plumbing is
/// owed here. `async` only shares the platform function's signature.
///
/// rfd's blocking save folds cancel and backend errors into one `None`,
/// so `None` is quiet-cancel; only a backend panic — caught, never
/// propagated — asks for the direct fallback.
#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn platform_portal_save_file(current_name: &str) -> PortalOutcome {
    let name = current_name.to_string();
    let picked = std::panic::catch_unwind(move || {
        rfd::FileDialog::new()
            .set_title("Save export")
            .set_file_name(name)
            .save_file()
    });
    native_save_outcome(picked)
}

/// Fold a finished native dialog into the shared outcome (#154): a picked
/// path is granted, a dismissal is quiet-cancel, and a backend panic (the
/// only failure rfd surfaces distinctly) asks for the direct fallback.
#[cfg(any(test, target_os = "windows", target_os = "macos"))]
pub(crate) fn native_save_outcome(
    picked: std::thread::Result<Option<std::path::PathBuf>>,
) -> PortalOutcome {
    match picked {
        Ok(Some(path)) => PortalOutcome::Granted(path.to_string_lossy().into_owned()),
        Ok(None) => PortalOutcome::Cancelled,
        Err(_) => PortalOutcome::Unavailable,
    }
}

/// Freedesktop portals are the Linux system dialog (§16), rfd the native
/// one on Windows and macOS (#154): remaining targets take the direct
/// home-directory fallback, which is what Unavailable requests.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
async fn platform_portal_save_file(_current_name: &str) -> PortalOutcome {
    PortalOutcome::Unavailable
}

#[cfg(target_os = "linux")]
fn picked_uri_outcome(files: ashpd::desktop::file_chooser::SelectedFiles) -> PortalOutcome {
    let Some(uri) = files.uris().first() else {
        return PortalOutcome::Cancelled;
    };
    // ashpd 0.14 hands back a string URI; parse and restrict to local file paths.
    let path = url::Url::parse(uri.as_str())
        .ok()
        .and_then(|parsed| parsed.to_file_path().ok());
    match path {
        Some(path) => PortalOutcome::Granted(path.to_string_lossy().into_owned()),
        None => PortalOutcome::Unavailable,
    }
}

#[cfg(target_os = "linux")]
async fn platform_portal_save_file(current_name: &str) -> PortalOutcome {
    let request = ashpd::desktop::file_chooser::SelectedFiles::save_file()
        .title("Save export")
        .current_name(current_name)
        .send()
        .await;
    match request {
        Ok(request) => match request.response() {
            Ok(files) => picked_uri_outcome(files),
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                PortalOutcome::Cancelled
            }
            Err(_) => PortalOutcome::Unavailable,
        },
        Err(_) => PortalOutcome::Unavailable,
    }
}

/// Ask the system chooser for a folder to sync with (#176).
///
/// Follows the same contract as [`portal_save_file`]: granted on confirm,
/// quiet cancel on dismiss, unavailable when no desktop portal answers (or
/// when the headless portal stub is armed).
pub(crate) async fn portal_pick_folder() -> PortalOutcome {
    #[cfg(feature = "portal-stub")]
    if std::env::var_os("TIMETRACK_PORTAL_STUB").is_some() {
        return PortalOutcome::Unavailable;
    }
    platform_portal_pick_folder().await
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn platform_portal_pick_folder() -> PortalOutcome {
    let picked = std::panic::catch_unwind(|| {
        rfd::FileDialog::new()
            .set_title("Choose sync folder")
            .pick_folder()
    });
    native_save_outcome(picked)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
async fn platform_portal_pick_folder() -> PortalOutcome {
    PortalOutcome::Unavailable
}

#[cfg(target_os = "linux")]
async fn platform_portal_pick_folder() -> PortalOutcome {
    let request = ashpd::desktop::file_chooser::SelectedFiles::open_file()
        .title("Choose sync folder")
        .directory(true)
        .send()
        .await;
    match request {
        Ok(request) => match request.response() {
            Ok(files) => picked_uri_outcome(files),
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                PortalOutcome::Cancelled
            }
            Err(_) => PortalOutcome::Unavailable,
        },
        Err(_) => PortalOutcome::Unavailable,
    }
}

/// Quiet status after cancelling the system save dialog (ticket 28).
///
/// A no-op, never an error: changing your mind is not a failure, so this
/// travels as `Msg::Status`, writes nothing, and names no path.
pub(crate) fn export_cancelled_status() -> &'static str {
    "export cancelled"
}

/// The cancel outcome as one message: a quiet status, never error-styled.
pub(crate) fn export_cancel_msg() -> Msg {
    Msg::Status(export_cancelled_status().to_string())
}

/// Write fetched CSV bytes to `dest` and report the outcome as one message.
///
/// The granted portal path and the direct fallback share this: success
/// names count, scope, week or month id and file base name in the shared
/// core wording, while a failed write stays a local note with path and
/// reason, never a service error. An existing destination is overwritten:
/// the portal's own overwrite question already governed it, so there is
/// exactly one prompt, never a second app confirm. The scope travels typed
/// (nothing re-parses it); `via_portal` records whether the portal granted
/// the path so the Export tab can label a chosen path differently from the
/// direct fallback.
pub(crate) fn export_write_msg(
    csv: &str,
    dest: &str,
    scope: timetrack_core::ExportScope,
    period_id: &str,
    via_portal: bool,
) -> Msg {
    let basename = export_basename(dest);
    let status = export_saved_status(entry_count(csv), scope, period_id, basename);
    write_export_csv(dest, csv, status, via_portal)
}

/// Write fetched range CSV bytes to `dest` and report the outcome as one
/// message (#133). The status is the CLI's range wording verbatim —
/// `exported {count} entries ({from}..{to}) to {basename}` — so one range
/// reads the same in both clients; a failed write stays a local note with
/// path and reason, exactly like the scoped save.
pub(crate) fn export_range_write_msg(
    csv: &str,
    dest: &str,
    period_id: &str,
    via_portal: bool,
) -> Msg {
    let basename = export_basename(dest);
    let status = export_range_saved_status(entry_count(csv), period_id, basename);
    write_export_csv(dest, csv, status, via_portal)
}

/// The shared write behind both saves: the bytes go through, success
/// reports the given status, and a failed write stays a local note naming
/// path plus reason. An existing destination is overwritten: the portal's
/// own overwrite question already governed it.
fn write_export_csv(dest: &str, csv: &str, status: String, via_portal: bool) -> Msg {
    match std::fs::write(dest, csv) {
        Ok(()) => Msg::ExportSaved {
            status,
            dest: dest.to_string(),
            via_portal,
        },
        Err(e) => Msg::Error(StatusMsg {
            text: format!("could not write {dest}: {e}"),
            kind: StatusKind::Local,
        }),
    }
}

/// Rows in a fetched CSV: the header plus one row per entry.
fn entry_count(csv: &str) -> usize {
    csv.lines().count().saturating_sub(1)
}

/// Where a GUI export lands while the portal is unreachable (ticket 27
/// fallback).
///
/// The app runs unsandboxed, so the home directory below is the real one.
/// The name carries the week or month id so one week never silently
/// overwrites the last; all time ignores the anchor and keeps the plain
/// name.
pub(crate) fn default_export_path(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    let name = export_suggested_name(scope, anchor_ms, tz);
    format!("{}/{}", fallback_home_dir(), name)
}

/// Where a custom-range export lands while the portal is unreachable
/// (#133): the same home-directory fallback, with the range id in the
/// name so ranges never overwrite each other or a scoped export.
pub(crate) fn default_export_range_path(
    from_ms: i64,
    to_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    let name = export_range_suggested_name(from_ms, to_ms, tz);
    format!("{}/{}", fallback_home_dir(), name)
}

/// The home directory the direct-save fallback writes into (#154).
///
/// `HOME` everywhere except Windows, where `HOME` is usually unset and
/// `USERPROFILE` names the real home; the old code fell through to `/tmp`
/// there, which does not exist, so every fallback save failed. Last resort
/// keeps the old answers: `/tmp` off Windows (byte-identical), the temp
/// dir on Windows. The `windows` flag is `cfg!(target_os = "windows")` at
/// the one call site and a test knob everywhere else.
pub(crate) fn fallback_home_dir() -> String {
    let home = std::env::var("HOME").ok();
    let profile = std::env::var("USERPROFILE").ok();
    fallback_home_from_env(
        home.as_deref(),
        profile.as_deref(),
        cfg!(target_os = "windows"),
    )
}

/// Pick the fallback home from the environment (#154): `USERPROFILE` wins
/// on Windows (where `HOME` is usually unset), `HOME` elsewhere; with
/// neither, the temp dir on Windows, `/tmp` elsewhere.
pub(crate) fn fallback_home_from_env(
    home: Option<&str>,
    userprofile: Option<&str>,
    windows: bool,
) -> String {
    if windows {
        userprofile
            .or(home)
            .map(str::to_string)
            .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned())
    } else {
        home.map(str::to_string)
            .unwrap_or_else(|| "/tmp".to_string())
    }
}

/// The file base name of a saved export path, for the status line (#154).
///
/// The native Windows dialog hands back `\`-separated paths, so both
/// separators cut there; elsewhere `/` alone, byte-identical to the old
/// inline split. The `windows` flag is `cfg!(target_os = "windows")` at
/// the one call site and a test knob everywhere else.
pub(crate) fn export_basename(dest: &str) -> &str {
    export_basename_impl(dest, cfg!(target_os = "windows"))
}

/// Cut the base name off a saved export path (#154).
pub(crate) fn export_basename_impl(dest: &str, windows: bool) -> &str {
    if windows {
        dest.rsplit(['/', '\\']).next().unwrap_or(dest)
    } else {
        dest.rsplit('/').next().unwrap_or(dest)
    }
}

/// The name prefilled in the system save dialog for a custom range
/// (#133): the range id, so one range never silently overwrites another.
pub(crate) fn export_range_suggested_name(
    from_ms: i64,
    to_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    format!(
        "timetrack-export-range-{}.csv",
        export_range_period_id(from_ms, to_ms, tz)
    )
}

/// Stable identity of a custom range (#133): the two local days joined by
/// `..`, e.g. `2026-10-01..2026-10-15` — the period semantics the issue
/// asks for, and the same pair the CLI prints in its range status line.
pub(crate) fn export_range_period_id(from_ms: i64, to_ms: i64, tz: &timetrack_core::Tz) -> String {
    format!(
        "{}..{}",
        timetrack_core::anchor_day_string(from_ms, tz),
        timetrack_core::anchor_day_string(to_ms, tz)
    )
}

/// The label for the export buttons: without a portal the app writes
/// directly to the home directory, and the system save dialog (ticket 27)
/// is what offers any other location.
pub(crate) fn export_location_label() -> &'static str {
    "Direct save: writes to ~/"
}

/// The Export tab's location line given the last save: a portal grant wrote
/// to the chosen path, so it says so with the path; anything else (direct
/// save or no save yet) keeps the home-directory wording.
pub(crate) fn export_location_text(last: Option<(&str, bool)>) -> String {
    match last {
        Some((dest, true)) => format!("Saved via system dialog: {dest}"),
        _ => export_location_label().to_string(),
    }
}

/// Status line after an interim save: entry count, scope, week or month id,
/// and file base name, in the shared core wording.
pub(crate) fn export_saved_status(
    entry_count: usize,
    scope: timetrack_core::ExportScope,
    period_id: &str,
    basename: &str,
) -> String {
    timetrack_core::format_export_status(entry_count, scope, period_id, basename)
}

/// Status line after a saved range export (#133): entry count, the range
/// id, and file base name. The CLI's `--out` status wording verbatim —
/// `exported {count} entries ({from}..{to}) to {basename}` — so one range
/// reads the same in both clients.
pub(crate) fn export_range_saved_status(
    entry_count: usize,
    period_id: &str,
    basename: &str,
) -> String {
    format!("exported {entry_count} entries ({period_id}) to {basename}")
}

/// Which service call an interim save uses: `None` is the old scope-only
/// call (anchored at now by the service), `Some(anchor)` the scope-plus-
/// anchor call. All time always takes the old call however stale the anchor
/// is, and a current week or month does too, so old services keep serving
/// the common case; only a chosen week or month needs the new call.
pub(crate) fn export_anchor_for_save(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    now_ms: i64,
    tz: &timetrack_core::Tz,
) -> Option<i64> {
    match scope {
        timetrack_core::ExportScope::All => None,
        _ if scope.period_id(anchor_ms, tz) == scope.period_id(now_ms, tz) => None,
        _ => Some(anchor_ms),
    }
}

/// The rows the scoped export will hold, oldest first, over the snapshot's
/// entries with the snapshot zone (spec #19).
///
/// Filters with the shared core predicate ([`timetrack_core::entry_in_scope`]:
/// `started_at` wins per ADR-0002, all time ignores the anchor), so the
/// preview shows what the file will hold without a second service call and
/// cannot disagree with the service bucketing. The snapshot arrives newest
/// first; the preview re-sorts oldest first, the order a stakeholder reads.
pub(crate) fn export_preview_entries<'a>(
    snapshot: &'a Snapshot,
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> Vec<&'a timetrack_proto::EntryView> {
    let mut out: Vec<&timetrack_proto::EntryView> = snapshot
        .entries
        .iter()
        .filter(|e| timetrack_core::entry_in_scope(e.started_at, scope, anchor_ms, tz))
        .collect();
    out.sort_by_key(|e| e.started_at);
    out
}

/// The rows a custom-range export will hold, oldest first (#133).
///
/// The same predicate core's [`timetrack_core::export_csv_between`] uses —
/// each entry buckets by its `started_at` local day, inclusive at both
/// ends — so the preview shows what the file will hold without a second
/// service call and cannot disagree with it.
pub(crate) fn export_preview_entries_between<'a>(
    snapshot: &'a Snapshot,
    from_ms: i64,
    to_ms: i64,
    tz: &timetrack_core::Tz,
) -> Vec<&'a timetrack_proto::EntryView> {
    let (from_day, to_day) = (tz.day_of(from_ms), tz.day_of(to_ms));
    let mut out: Vec<&timetrack_proto::EntryView> = snapshot
        .entries
        .iter()
        .filter(|e| {
            let day = tz.day_of(e.started_at);
            day >= from_day && day <= to_day
        })
        .collect();
    out.sort_by_key(|e| e.started_at);
    out
}

#[cfg(test)]
mod tests {
    use super::super::picker::export_period_label;
    use super::*;
    use crate::app::testsupport::{
        day_anchor, preview_ids, preview_range_ids, preview_snapshot, utc_tz,
    };

    #[test]
    fn location_line_names_a_portal_grant_path() {
        // After a portal grant the tab names the chosen path it wrote to;
        // fallback saves (and no save yet) keep the home-directory wording.
        assert_eq!(
            export_location_text(Some(("/home/user/chosen.csv", true))),
            "Saved via system dialog: /home/user/chosen.csv"
        );
        assert_eq!(
            export_location_text(Some(("/home/user/timetrack-export-all.csv", false))),
            export_location_label()
        );
        assert_eq!(export_location_text(None), export_location_label());
    }

    #[test]
    fn preview_lists_the_scoped_rows_oldest_first() {
        // The service sends newest first; the preview shows what the file
        // will hold: the anchored week's rows oldest first.
        let snap = preview_snapshot();
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08")
            ),
            vec!["e2".to_string(), "e3".to_string()]
        );
    }

    #[test]
    fn preview_changes_with_the_anchor() {
        let snap = preview_snapshot();
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Week,
                day_anchor("2026-09-30")
            ),
            vec!["e1".to_string()]
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-30")
            ),
            vec!["e1".to_string()]
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Month,
                day_anchor("2026-10-08")
            ),
            vec!["e2".to_string(), "e3".to_string()]
        );
    }

    #[test]
    fn all_time_preview_ignores_the_anchor() {
        // A stale anchor must never narrow a full export, on screen or in
        // the file: both anchors preview every entry oldest first.
        let snap = preview_snapshot();
        let all = vec!["e1".to_string(), "e2".to_string(), "e3".to_string()];
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-30")
            ),
            all
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::All,
                day_anchor("2026-10-08")
            ),
            all
        );
    }

    #[test]
    fn interim_save_uses_period_stamped_names() {
        // One file per week or month so weeks never overwrite each other;
        // all time keeps the plain name whatever the anchor is.
        let week = default_export_path(
            timetrack_core::ExportScope::Week,
            day_anchor("2026-10-08"),
            &utc_tz(),
        );
        assert!(
            week.ends_with("timetrack-export-week-2026-W41.csv"),
            "week file carries its week id: {week}"
        );
        let month = default_export_path(
            timetrack_core::ExportScope::Month,
            day_anchor("2026-09-15"),
            &utc_tz(),
        );
        assert!(
            month.ends_with("timetrack-export-month-2026-09.csv"),
            "month file carries its month id: {month}"
        );
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            let all = default_export_path(timetrack_core::ExportScope::All, anchor, &utc_tz());
            assert!(
                all.ends_with("timetrack-export-all.csv"),
                "all time ignores the anchor: {all}"
            );
        }
    }

    #[test]
    fn interim_location_is_labelled_home_dir() {
        // The app runs unsandboxed, so the fallback writes to the real
        // home: the label must say so until the portal dialog lands.
        let label = export_location_label();
        assert!(label.contains("~/"), "label names the home dir: {label}");
        assert!(
            !label.contains("sandbox"),
            "label never claims a sandbox: {label}"
        );
    }

    #[test]
    fn saved_status_names_count_scope_period_and_name() {
        // End to end of the interim save wording: the period-stamped path's
        // basename plus the picker's period label, in the shared core
        // wording. The literals are the independent truth, not a recompute.
        let scope = timetrack_core::ExportScope::Week;
        let anchor = day_anchor("2026-10-08");
        let path = default_export_path(scope, anchor, &utc_tz());
        let basename = path.rsplit('/').next().unwrap();
        let period = export_period_label(scope, anchor, &utc_tz());
        assert_eq!(
            export_saved_status(2, scope, &period, basename),
            "exported 2 entries (week 2026-W41) to timetrack-export-week-2026-W41.csv"
        );
        let all_path = default_export_path(
            timetrack_core::ExportScope::All,
            day_anchor("2026-10-08"),
            &utc_tz(),
        );
        let all_basename = all_path.rsplit('/').next().unwrap();
        assert_eq!(
            export_saved_status(
                3,
                timetrack_core::ExportScope::All,
                "all time",
                all_basename
            ),
            "exported 3 entries (all time) to timetrack-export-all.csv"
        );
    }

    #[test]
    fn all_time_save_ignores_the_anchor() {
        // All time travels over the old scope-only call whatever the anchor
        // is, so old services keep serving full exports.
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            assert_eq!(
                export_anchor_for_save(
                    timetrack_core::ExportScope::All,
                    anchor,
                    day_anchor("2026-10-08"),
                    &utc_tz()
                ),
                None
            );
        }
    }

    #[test]
    fn current_scope_save_uses_the_old_call() {
        // The old call is anchored at now by the service, so a current
        // anchor needs no new call and keeps working against old services.
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                day_anchor("2026-10-06"),
                &utc_tz()
            ),
            None
        );
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-10-08"),
                day_anchor("2026-10-01"),
                &utc_tz()
            ),
            None
        );
    }

    #[test]
    fn chosen_scope_save_uses_the_anchored_call() {
        // A week or month outside the current one needs the scope-plus-anchor
        // call; against an old service that call refuses with a version
        // message rather than writing a wrong file.
        let anchor = day_anchor("2026-09-30");
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Week,
                anchor,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            Some(anchor)
        );
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Month,
                anchor,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            Some(anchor)
        );
    }

    #[test]
    fn portal_suggests_period_stamped_names() {
        // The system save dialog opens with the period-stamped default name
        // prefilled: the week or month id for scoped exports so one week
        // never silently overwrites the last, the plain name for all time
        // whatever the anchor is. The literals are the independent truth, not
        // a recompute of the core naming.
        assert_eq!(
            export_suggested_name(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_suggested_name(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "timetrack-export-month-2026-09.csv"
        );
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            assert_eq!(
                export_suggested_name(timetrack_core::ExportScope::All, anchor, &utc_tz()),
                "timetrack-export-all.csv"
            );
        }
    }

    #[test]
    fn confirmed_save_writes_through_the_granted_path() {
        // A confirmed dialog wins over the home-directory fallback: the
        // bytes go to the path the portal granted, never to the fallback.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Granted("/home/user/chosen.csv".to_string()),
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            Some("/home/user/chosen.csv".to_string())
        );
    }

    #[test]
    fn cancelled_save_is_a_quiet_no_op() {
        // Cancelling the dialog resolves to no destination at all: the
        // caller sends the quiet cancel status instead and writes nothing.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Cancelled,
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            None
        );
    }

    #[test]
    fn unavailable_portal_falls_back_to_the_home_dir_path() {
        // No portal (headless CI): the direct home-directory write runs on,
        // so the workflow never breaks where no dialog can open.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Unavailable,
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            Some("/home/user/timetrack-export-week-2026-W41.csv".to_string())
        );
    }

    #[test]
    fn granted_path_write_lands_bytes_and_reports_status() {
        // A confirmed save writes the already-fetched scoped CSV through the
        // granted path and reports count, scope, week id and file base name
        // in the shared core wording.
        let csv = format!(
            "{}\ne1,p1,review,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\ne2,p1,review,2026-10-07T12:00:00,2026-10-07T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        let dest =
            std::env::temp_dir().join(format!("tt-portal-granted-{}-week.csv", std::process::id()));
        let dest_str = dest.to_string_lossy().into_owned();
        let basename = dest.file_name().unwrap().to_string_lossy().into_owned();
        match export_write_msg(
            &csv,
            &dest_str,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            true,
        ) {
            Msg::ExportSaved {
                status,
                dest: saved_dest,
                via_portal,
            } => {
                assert_eq!(
                    status,
                    format!("exported 2 entries (week 2026-W41) to {basename}")
                );
                assert_eq!(saved_dest, dest_str);
                assert!(via_portal);
            }
            _ => panic!("a granted write reports success"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), csv);
        std::fs::remove_file(&dest).unwrap();
    }

    #[test]
    #[cfg(feature = "portal-stub")]
    fn portal_stub_ignores_anything_else() {
        assert_eq!(portal_stub_outcome("bogus", "x.csv"), None);
        assert_eq!(portal_stub_outcome("", "x.csv"), None);
    }

    #[test]
    #[cfg(feature = "portal-stub")]
    fn portal_stub_records_the_dialog_name() {
        // Headless-test seam: `record:<file>` logs the requested dialog
        // name and reports the portal unavailable, so the direct
        // fallback runs where no live portal exists.
        let log = std::env::temp_dir().join(format!("tt-portal-stub-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        assert_eq!(
            portal_stub_outcome(
                &format!("record:{}", log.to_string_lossy()),
                "timetrack-export-week-2026-W41.csv",
            ),
            Some(PortalOutcome::Unavailable)
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        std::fs::remove_file(&log).unwrap();
    }

    #[test]
    fn cancel_reports_a_quiet_status() {
        // Cancelling the save dialog is a quiet no-op status, never an
        // error: changing your mind is not a failure, so this travels as
        // `Msg::Status`, not `Msg::Error`, and writes nothing (the
        // `cancelled_save_is_a_quiet_no_op` pin above proves no destination).
        assert_eq!(export_cancelled_status(), "export cancelled");
        match export_cancel_msg() {
            Msg::Status(text) => assert_eq!(text, "export cancelled"),
            Msg::Error(_) => panic!("cancel must be a quiet status, not an error"),
            _ => panic!("cancel must be a quiet status"),
        }
    }

    #[test]
    fn existing_destination_is_overwritten_without_a_second_prompt() {
        // The portal's own overwrite question already governed an existing
        // destination, so the app writes straight through it: no existence
        // check, no second confirm, just the new bytes plus the saved status.
        let csv = format!(
            "{}\nnew,p1,review,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        let dest = std::env::temp_dir().join(format!(
            "tt-portal-overwrite-{}-week.csv",
            std::process::id()
        ));
        let dest_str = dest.to_string_lossy().into_owned();
        let basename = dest.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(&dest, "old bytes").unwrap();
        match export_write_msg(
            &csv,
            &dest_str,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            true,
        ) {
            Msg::ExportSaved { status, .. } => assert_eq!(
                status,
                format!("exported 1 entries (week 2026-W41) to {basename}")
            ),
            Msg::Error(_) => panic!("an existing destination must be overwritten, not refused"),
            _ => panic!("overwrite must report success"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), csv);
        std::fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn write_failure_is_a_local_error_naming_path_and_reason() {
        // A granted path that cannot be written and a failing direct
        // fallback share one outcome: a local error naming the path plus the
        // reason, never a service or validation failure.
        let dir = std::env::temp_dir().join(format!("tt-portal-fail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.to_string_lossy().into_owned();
        let csv = format!("{}\n", timetrack_core::CSV_HEADER);
        match export_write_msg(
            &csv,
            &dest,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            false,
        ) {
            Msg::Error(note) => {
                assert_eq!(note.kind, StatusKind::Local);
                assert!(
                    note.text.starts_with(&format!("could not write {dest}")),
                    "must name the path first: {}",
                    note.text
                );
                assert!(
                    note.text.contains(": "),
                    "must name the reason too: {}",
                    note.text
                );
                for banned in ["service", "validation", "invalid", "unknown", "version"] {
                    assert!(
                        !note.text.to_ascii_lowercase().contains(banned),
                        "must never read as a service or validation failure ({banned}): {}",
                        note.text
                    );
                }
            }
            Msg::Status(_) => panic!("an unwritable destination must report a local error"),
            _ => panic!("an unwritable destination must report a local error"),
        }
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    #[cfg(feature = "portal-stub")]
    fn portal_stub_cancel_flag_returns_cancelled() {
        // Headless cancel pin (no live portal): with `<log>.cancel` present,
        // the stub still logs the requested dialog name but answers
        // Cancelled, so the quiet no-op path runs without a dialog.
        let log = std::env::temp_dir().join(format!(
            "tt-portal-cancel-{}-{}.log",
            std::process::id(),
            "cancel-flag"
        ));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.cancel", log.to_string_lossy()));
        std::fs::write(format!("{}.cancel", log.to_string_lossy()), "").unwrap();
        assert_eq!(
            portal_stub_outcome(
                &format!("record:{}", log.to_string_lossy()),
                "timetrack-export-week-2026-W41.csv",
            ),
            Some(PortalOutcome::Cancelled)
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.cancel", log.to_string_lossy()));
    }

    #[test]
    #[cfg(feature = "portal-stub")]
    fn portal_stub_fail_flag_returns_an_unwritable_grant() {
        // Headless failure pin (no live portal): with `<log>.fail` present,
        // the stub still logs the requested dialog name but grants a path
        // that cannot be written, so the local-error path runs without a
        // dialog.
        let log = std::env::temp_dir().join(format!(
            "tt-portal-fail-{}-{}.log",
            std::process::id(),
            "fail-flag"
        ));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.fail", log.to_string_lossy()));
        std::fs::write(format!("{}.fail", log.to_string_lossy()), "").unwrap();
        match portal_stub_outcome(
            &format!("record:{}", log.to_string_lossy()),
            "timetrack-export-week-2026-W41.csv",
        ) {
            Some(PortalOutcome::Granted(dest)) => {
                assert!(
                    dest.contains("timetrack-export-week-2026-W41.csv"),
                    "granted path must carry the dialog name: {dest}"
                );
                assert!(
                    std::fs::write(&dest, "probe").is_err(),
                    "granted path must be unwritable: {dest}"
                );
            }
            _other => panic!("fail flag must grant an unwritable path"),
        }
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.fail", log.to_string_lossy()));
    }

    #[test]
    fn range_names_status_and_file_in_cli_wording() {
        // File naming (#133): the range id — the `..` pair the issue asks
        // for — so one range never silently overwrites another or a scoped
        // export. The literals are the independent truth, not a recompute.
        let from = day_anchor("2026-10-01");
        let to = day_anchor("2026-10-15");
        assert_eq!(
            export_range_period_id(from, to, &utc_tz()),
            "2026-10-01..2026-10-15"
        );
        assert_eq!(
            export_range_suggested_name(from, to, &utc_tz()),
            "timetrack-export-range-2026-10-01..2026-10-15.csv"
        );
        let path = default_export_range_path(from, to, &utc_tz());
        assert!(
            path.ends_with("timetrack-export-range-2026-10-01..2026-10-15.csv"),
            "the fallback path carries the range id: {path}"
        );
        // Status: the CLI's `--out` wording verbatim, so one range reads
        // the same in both clients.
        assert_eq!(
            export_range_saved_status(
                2,
                "2026-10-01..2026-10-15",
                "timetrack-export-range-2026-10-01..2026-10-15.csv"
            ),
            "exported 2 entries (2026-10-01..2026-10-15) to timetrack-export-range-2026-10-01..2026-10-15.csv"
        );
    }

    #[test]
    fn range_preview_agrees_with_core_export_between() {
        // Preview parity (#133): the preview holds exactly the rows the
        // file will hold — the same ids in the same oldest-first order as
        // core's `export_csv_between` over the same days, starting at win.
        let snap = preview_snapshot();
        let from = day_anchor("2026-09-30");
        let to = day_anchor("2026-10-15");
        let store = timetrack_core::Store {
            version: timetrack_core::STORE_VERSION,
            projects: vec![],
            entries: snap
                .entries
                .iter()
                .map(|e| timetrack_core::Entry {
                    id: e.id.clone(),
                    project_id: e.project_id.clone(),
                    description: e.description.clone(),
                    started_at: e.started_at,
                    ended_at: e.ended_at,
                    source: match e.source {
                        timetrack_proto::EntrySource::Manual => timetrack_core::EntrySource::Manual,
                        timetrack_proto::EntrySource::QuickAdd => {
                            timetrack_core::EntrySource::QuickAdd
                        }
                        timetrack_proto::EntrySource::External => {
                            timetrack_core::EntrySource::External
                        }
                    },
                    note: e.note.clone(),
                })
                .collect(),
        };
        let csv = timetrack_core::export_csv_between(&store, from, to, &utc_tz());
        let file_ids: Vec<String> = csv
            .lines()
            .skip(1)
            .map(|line| line.split(',').next().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            preview_range_ids(&snap, from, to),
            file_ids,
            "the preview shows what the file will hold"
        );
        assert_eq!(
            preview_range_ids(&snap, from, to),
            vec!["e1".to_string(), "e2".to_string(), "e3".to_string()]
        );
        // Boundary days are inclusive by start day, like the scoped
        // exports: an entry starting before the range stays out even when
        // it ends inside it.
        assert_eq!(
            preview_range_ids(&snap, day_anchor("2026-10-06"), day_anchor("2026-10-06")),
            vec!["e2".to_string()]
        );
        assert_eq!(
            preview_range_ids(&snap, day_anchor("2026-10-07"), day_anchor("2026-09-30")),
            Vec::<String>::new(),
            "a backwards range previews nothing, like the empty file"
        );
    }

    #[test]
    fn range_write_reports_cli_wording() {
        // A confirmed range save writes the fetched range CSV and reports
        // count, range id and file base name in the CLI's wording.
        let csv = format!(
            "{}\ne2,p1,review,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        let dest = std::env::temp_dir().join(format!(
            "tt-portal-granted-{}-range.csv",
            std::process::id()
        ));
        let dest_str = dest.to_string_lossy().into_owned();
        let basename = dest.file_name().unwrap().to_string_lossy().into_owned();
        match export_range_write_msg(&csv, &dest_str, "2026-10-01..2026-10-15", true) {
            Msg::ExportSaved {
                status,
                dest: saved_dest,
                via_portal,
            } => {
                assert_eq!(
                    status,
                    format!("exported 1 entries (2026-10-01..2026-10-15) to {basename}")
                );
                assert_eq!(saved_dest, dest_str);
                assert!(via_portal);
            }
            _ => panic!("a granted range write reports success"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), csv);
        std::fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn native_dialog_folds_pick_cancel_and_panic() {
        // The native-dialog outcome fold (#154): a picked path is granted
        // (and resolves to the picked destination, never the fallback), a
        // dismissal is quiet-cancel (no destination, writes nothing), and
        // a backend panic asks for the direct fallback. rfd folds cancel
        // and backend errors into one `None`, so only the panic arm can
        // request the fallback.
        let picked: std::thread::Result<Option<std::path::PathBuf>> =
            Ok(Some(std::path::PathBuf::from("/home/user/chosen.csv")));
        assert_eq!(
            native_save_outcome(picked),
            PortalOutcome::Granted("/home/user/chosen.csv".to_string())
        );
        let dismissed: std::thread::Result<Option<std::path::PathBuf>> = Ok(None);
        assert_eq!(native_save_outcome(dismissed), PortalOutcome::Cancelled);
        assert_eq!(
            resolve_export_destination(PortalOutcome::Cancelled, "/home/user/fallback.csv"),
            None
        );
        let panicked: std::thread::Result<Option<std::path::PathBuf>> =
            Err(Box::new("backend blew up"));
        assert_eq!(
            resolve_export_destination(native_save_outcome(panicked), "/home/user/fallback.csv"),
            Some("/home/user/fallback.csv".to_string())
        );
    }

    #[test]
    fn fallback_home_prefers_userprofile_on_windows() {
        // Windows has no `HOME` in practice: `USERPROFILE` names the real
        // home, `HOME` is the backup, and the temp dir is the last resort
        // — never `/tmp`, which does not exist there.
        assert_eq!(
            fallback_home_from_env(None, Some("C:\\Users\\ada"), true),
            "C:\\Users\\ada"
        );
        assert_eq!(
            fallback_home_from_env(Some("/home/ada"), Some("C:\\Users\\ada"), true),
            "C:\\Users\\ada"
        );
        assert_eq!(
            fallback_home_from_env(Some("/home/ada"), None, true),
            "/home/ada"
        );
        let last_resort = fallback_home_from_env(None, None, true);
        assert_eq!(
            last_resort,
            std::env::temp_dir().to_string_lossy().into_owned()
        );
        assert!(!last_resort.is_empty());
    }

    #[test]
    fn fallback_home_keeps_home_and_tmp_off_windows() {
        // Off Windows the fallback is byte-identical to before #154: `HOME`
        // when set (a stray `USERPROFILE` is ignored), `/tmp` when not.
        assert_eq!(
            fallback_home_from_env(Some("/home/ada"), Some("C:\\Users\\ada"), false),
            "/home/ada"
        );
        assert_eq!(fallback_home_from_env(None, None, false), "/tmp");
        assert_eq!(
            fallback_home_from_env(None, Some("C:\\Users\\ada"), false),
            "/tmp"
        );
    }

    #[test]
    fn basename_cuts_backslash_only_on_windows() {
        // The native Windows dialog hands back `\`-separated paths, so the
        // status line cuts both separators there — and `/` alone elsewhere,
        // where a backslash is an ordinary filename character.
        assert_eq!(
            export_basename_impl("C:\\Users\\ada\\timetrack-export-week-2026-W41.csv", true),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_basename_impl("C:/Users/ada/timetrack-export-week-2026-W41.csv", true),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_basename_impl("/home/ada/timetrack-export-week-2026-W41.csv", false),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_basename_impl("weird\\name.csv", false),
            "weird\\name.csv"
        );
    }
}
