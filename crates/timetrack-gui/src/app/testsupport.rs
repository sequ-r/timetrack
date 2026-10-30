/* gui/app/testsupport.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The fixtures the GUI's tests share.
//!
//! Several groups of tests need the same builders -- an entry, a project, a
//! snapshot with a known shape -- and they used to live in the test module
//! at the bottom of `app.rs`. They are here instead, so a test that moves
//! into a module of its own can still reach them.

use super::dialogs::entry::EntryDialog;
use super::dialogs::project::ProjectPrompt;
use super::dialogs::split::SplitPrompt;
use super::export::save::export_preview_entries;
use super::status::StatusMsg;
use timetrack_proto::{ClientError, EntrySource, EntryView, ProjectView, Snapshot};

/// One entry on `p1`, an hour long, with the given source.
pub(super) fn entry(id: &str, source: EntrySource, start: i64) -> EntryView {
    EntryView {
        id: id.into(),
        project_id: "p1".into(),
        description: id.into(),
        started_at: start,
        ended_at: start + 3_600_000,
        source,
        note: None,
    }
}

/// One project.
pub(super) fn project(id: &str, name: &str) -> ProjectView {
    ProjectView {
        id: id.into(),
        name: name.into(),
        colour: None,
        archived: false,
    }
}

/// One manually entered entry, on the given project and with the given
/// endpoints.
pub(super) fn entry_on(id: &str, project: &str, start: i64, end: i64) -> EntryView {
    EntryView {
        id: id.into(),
        project_id: project.into(),
        description: id.into(),
        started_at: start,
        ended_at: end,
        source: EntrySource::Manual,
        note: None,
    }
}

/// The footer status line a client error turns into.
pub(super) fn status_for(error: &ClientError) -> StatusMsg {
    StatusMsg::from(error)
}

/// The zone the export fixtures are anchored in.
pub(super) fn utc_tz() -> timetrack_core::Tz {
    timetrack_core::Tz::utc()
}

/// The instant of a typed day, in the zone above.
pub(super) fn day_anchor(text: &str) -> i64 {
    timetrack_core::parse_anchor_day(text, "anchor", &utc_tz()).unwrap()
}

/// Four entries: two overlapping on `p1`, one disjoint on `p1`, and one
/// overlapping the first but on `p2`.
pub(super) fn merge_snapshot() -> Snapshot {
    // e1 [0h,1h] and e2 [0.5h,1.5h] overlap on p1; e3 is disjoint on p1;
    // e4 overlaps e1 but lives on p2 and must never join.
    Snapshot {
        entries: vec![
            entry_on("e1", "p1", 0, 3_600_000),
            entry_on("e2", "p1", 1_800_000, 5_400_000),
            entry_on("e3", "p1", 10 * 3_600_000, 11 * 3_600_000),
            entry_on("e4", "p2", 0, 3_600_000),
        ],
        projects: vec![project("p1", "Work"), project("p2", "Home")],
        ..Default::default()
    }
}

/// Three entries, newest first as the service sends them: one in 2026-W40
/// (30 Sep) and two in 2026-W41 (6 and 7 Oct).
pub(super) fn preview_snapshot() -> Snapshot {
    Snapshot {
        entries: vec![
            entry_on(
                "e3",
                "p1",
                day_anchor("2026-10-07"),
                day_anchor("2026-10-07") + 3_600_000,
            ),
            entry_on(
                "e2",
                "p1",
                day_anchor("2026-10-06"),
                day_anchor("2026-10-06") + 3_600_000,
            ),
            entry_on(
                "e1",
                "p1",
                day_anchor("2026-09-30"),
                day_anchor("2026-09-30") + 3_600_000,
            ),
        ],
        ..Default::default()
    }
}

/// The ids the scoped export preview holds, oldest first.
pub(super) fn preview_ids(
    snap: &Snapshot,
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
) -> Vec<String> {
    export_preview_entries(snap, scope, anchor_ms, &utc_tz())
        .iter()
        .map(|e| e.id.clone())
        .collect()
}

/// A new-entry dialog for `p1` ("Work").
pub(super) fn new_dialog() -> EntryDialog {
    EntryDialog::new("p1".into(), "Work".into())
}

/// A split prompt on `e1`, with the given typed split point.
pub(super) fn split_prompt(at: &str) -> SplitPrompt {
    SplitPrompt {
        entry_id: "e1".into(),
        started_at: 0,
        ended_at: 3_600_000,
        caret: crate::app::textline::Caret::at_end(at),
        at: at.into(),
        error: None,
    }
}

/// A project prompt that creates (`edit_id` none) or renames (`edit_id`
/// set) a project.
pub(super) fn project_prompt(edit_id: Option<&str>, name: &str) -> ProjectPrompt {
    ProjectPrompt {
        edit_id: edit_id.map(str::to_string),
        caret: crate::app::textline::Caret::at_end(name),
        name: name.into(),
        error: None,
    }
}
