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

use super::super::service::{ExportRequest, Msg};
use super::super::status::{StatusKind, StatusMsg};
use std::sync::mpsc::Sender;
use timetrack_proto::Snapshot;

/// Ask where to put an export and write it there.
///
/// Runs off the service thread, so the save dialog never stops that thread
/// from serving: the window keeps painting and refreshing, and a quick-add
/// made while the dialog is open still lands in the store.
pub(crate) async fn save_export(tx: Sender<Msg>, csv: String, req: ExportRequest) {
    // The dialog opens after the fetch, so a service refusal never prompts
    // for a location. Cancel is a quiet status that writes nothing; no
    // portal falls back to the direct home-directory write.
    let outcome = portal_save_file(&req.suggested_name).await;
    let via_portal = matches!(outcome, PortalOutcome::Granted(_));
    match resolve_export_destination(outcome, &req.path) {
        Some(dest) => {
            let _ = tx.send(export_write_msg(
                &csv,
                &dest,
                req.scope,
                &req.period_id,
                via_portal,
            ));
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
/// overwrites the last; the portal's own overwrite question governs existing
/// files, so there is exactly one prompt. Cancel is quiet (`Cancelled`);
/// anything but a confirm or a cancel — no portal frontend, no session bus
/// — counts as unavailable and the direct fallback runs.
pub(crate) async fn portal_save_file(current_name: &str) -> PortalOutcome {
    if let Some(outcome) = stubbed_outcome(current_name) {
        return outcome;
    }
    let request = ashpd::desktop::file_chooser::SelectedFiles::save_file()
        .title("Save export")
        .current_name(current_name)
        .send()
        .await;
    let files = match request {
        Ok(request) => match request.response() {
            Ok(files) => files,
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                return PortalOutcome::Cancelled;
            }
            Err(_) => return PortalOutcome::Unavailable,
        },
        Err(_) => return PortalOutcome::Unavailable,
    };
    let Some(uri) = files.uris().first() else {
        return PortalOutcome::Cancelled;
    };
    // ashpd 0.13 hands back a string URI; parsing it as a file URI (and
    // refusing anything that is not a local path) is what `url` does here,
    // exactly as ashpd 0.11's `Uri::to_file_path` did on our behalf.
    let path = url::Url::parse(uri.as_str())
        .ok()
        .and_then(|parsed| parsed.to_file_path().ok());
    match path {
        Some(path) => PortalOutcome::Granted(path.to_string_lossy().into_owned()),
        None => PortalOutcome::Unavailable,
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
    match std::fs::write(dest, csv) {
        Ok(()) => {
            let count = csv.lines().count().saturating_sub(1);
            let basename = dest.rsplit('/').next().unwrap_or(dest);
            let status = export_saved_status(count, scope, period_id, basename);
            Msg::ExportSaved {
                status,
                dest: dest.to_string(),
                via_portal,
            }
        }
        Err(e) => Msg::Error(StatusMsg {
            text: format!("could not write {dest}: {e}"),
            kind: StatusKind::Local,
        }),
    }
}

/// Where a GUI export lands while the portal is unreachable (ticket 27
/// fallback).
///
/// The app runs unsandboxed, so `$HOME` is the real home directory.
/// The name carries the week or month id so one week never silently
/// overwrites the last; all time ignores the anchor and keeps the plain
/// name.
pub(crate) fn default_export_path(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let name = export_suggested_name(scope, anchor_ms, tz);
    format!("{home}/{name}")
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

#[cfg(test)]
mod tests {
    use super::super::picker::export_period_label;
    use super::*;
    use crate::app::testsupport::{day_anchor, preview_ids, preview_snapshot, utc_tz};

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
}
