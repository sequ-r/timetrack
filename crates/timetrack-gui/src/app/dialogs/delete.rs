/* gui/app/dialogs/delete.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The delete confirmation: exactly which entry would go, so a destructive
//! action is never one keystroke with no reading step.
//!
//! Free over the snapshot like `merge_offer`, for the same reason.

use timetrack_proto::Snapshot;

/// A pending delete confirmation: the id to send, and the one-line summary
/// the banner shows (entry, project, duration) so `d` can never remove time
/// as a side effect of reaching for another key.
#[derive(Debug, Clone)]
pub(crate) struct DeleteConfirm {
    pub(crate) id: String,
    pub(crate) summary: String,
}

/// Build the delete confirmation for the selection, if there is one.
///
/// Free over the snapshot like `merge_offer`, for the same reason. The
/// summary names the entry, its project and its duration so the confirm
/// banner states exactly what would be lost.
pub(crate) fn delete_offer(snapshot: &Snapshot, selected: usize) -> Option<DeleteConfirm> {
    let picked = snapshot.entries.get(selected)?.clone();
    // Sanitized (#101): the summary is a one-line banner built from stored
    // text, which a legacy store can fill with controls.
    let project = snapshot
        .projects
        .iter()
        .find(|p| p.id == picked.project_id)
        .map(|p| timetrack_core::sanitize_display(&p.name))
        .unwrap_or_else(|| picked.project_id.clone());
    let what = if picked.description.is_empty() {
        "no note".to_string()
    } else {
        format!(
            "'{}'",
            timetrack_core::sanitize_display(&picked.description)
        )
    };
    Some(DeleteConfirm {
        summary: format!(
            "DELETE {} ({} · {} · {})? d to confirm, esc to cancel",
            picked.id,
            project,
            timetrack_core::format_duration(picked.duration_ms()),
            what,
        ),
        id: picked.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::{entry_on, project};
    use timetrack_proto::EntryView;

    #[test]
    fn delete_offer_names_the_entry_project_and_duration() {
        let snap = Snapshot {
            entries: vec![EntryView {
                description: "review".into(),
                ..entry_on("e1", "p1", 0, 3_600_000)
            }],
            projects: vec![project("p1", "Work")],
            ..Default::default()
        };
        let offer = delete_offer(&snap, 0).expect("e1 is selected");
        assert_eq!(offer.id, "e1");
        assert!(
            offer.summary.contains("DELETE e1"),
            "must name the entry: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("Work"),
            "must name the project: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("01:00:00"),
            "must state the duration: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("d to confirm"),
            "must state the confirm key: {}",
            offer.summary
        );
    }

    #[test]
    fn delete_offer_sanitizes_stored_text() {
        // #101: the banner is built from stored name/description, which a
        // legacy store can fill with controls the current rules refuse.
        let snap = Snapshot {
            entries: vec![EntryView {
                description: "rev\niew\u{1b}[31m".into(),
                ..entry_on("e1", "p1", 0, 3_600_000)
            }],
            projects: vec![project("p1", "Wo\nrk")],
            ..Default::default()
        };
        let offer = delete_offer(&snap, 0).unwrap();
        assert!(
            !offer.summary.chars().any(|c| c.is_control()),
            "{}",
            offer.summary
        );
        assert!(offer.summary.contains("Wo rk"), "{}", offer.summary);
        assert!(offer.summary.contains("rev iew [31m"), "{}", offer.summary);
    }

    #[test]
    fn delete_offer_is_none_without_a_selection() {
        assert!(delete_offer(&Snapshot::default(), 0).is_none());
        let snap = Snapshot {
            entries: vec![entry_on("e1", "p1", 0, 3_600_000)],
            projects: vec![project("p1", "Work")],
            ..Default::default()
        };
        assert!(delete_offer(&snap, 99).is_none());
    }
}
