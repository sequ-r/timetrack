/* gui/app/dialogs/delete_project.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The delete-project confirmation (#104): exactly which project would go,
//! behind the same two-step guard as entry deletion — the first `D` (or
//! the row's Delete button) only opens the banner, a second `D` executes.
//!
//! Free over the snapshot like `delete_offer`, for the same reason. The
//! pre-checks mirror the service's guards using the core's own error
//! wording, so the banner is never offered for a delete the service would
//! refuse anyway; the service still re-validates on execute.

use timetrack_proto::Snapshot;

/// A pending project deletion: the id to send, and the one-line summary
/// the banner shows.
#[derive(Debug, Clone)]
pub(crate) struct DeleteProjectConfirm {
    pub(crate) id: String,
    pub(crate) summary: String,
}

/// Build the delete confirmation for a project, or say why not.
///
/// `Err` carries the refusal in the core's own words (constructed from the
/// same `RuleError` variants the service would report), for the footer.
/// The guards mirror `timetrack_core::delete_project`: the last project
/// and a project with entries both refuse.
pub(crate) fn delete_project_offer(
    snapshot: &Snapshot,
    project_id: &str,
) -> Result<DeleteProjectConfirm, String> {
    let project = snapshot
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| {
            timetrack_core::RuleError::UnknownProject(project_id.to_string()).to_string()
        })?;
    if snapshot.projects.len() <= 1 {
        return Err(timetrack_core::RuleError::LastProject.to_string());
    }
    if snapshot.entries.iter().any(|e| e.project_id == project_id) {
        return Err(
            timetrack_core::RuleError::ProjectHasEntries(project_id.to_string()).to_string(),
        );
    }
    Ok(DeleteProjectConfirm {
        summary: format!(
            "DELETE PROJECT {} ({})? D to confirm, esc to cancel",
            timetrack_core::sanitize_display(&project.name),
            project.id,
        ),
        id: project.id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::{entry_on, project};

    fn snap() -> Snapshot {
        Snapshot {
            projects: vec![project("p1", "Work"), project("p2", "Empty")],
            entries: vec![entry_on("e1", "p1", 0, 3_600_000)],
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_project_is_offered_with_its_name() {
        let offer = delete_project_offer(&snap(), "p2").expect("p2 is deletable");
        assert_eq!(offer.id, "p2");
        assert!(
            offer.summary.contains("DELETE PROJECT Empty (p2)"),
            "{}",
            offer.summary
        );
        assert!(offer.summary.contains("D to confirm"), "{}", offer.summary);
    }

    #[test]
    fn a_project_with_entries_refuses_in_the_cores_words() {
        let err = delete_project_offer(&snap(), "p1").unwrap_err();
        assert_eq!(
            err,
            timetrack_core::RuleError::ProjectHasEntries("p1".into()).to_string()
        );
    }

    #[test]
    fn the_last_project_refuses_even_when_empty() {
        let snap = Snapshot {
            projects: vec![project("p1", "Work")],
            ..Default::default()
        };
        assert_eq!(
            delete_project_offer(&snap, "p1").unwrap_err(),
            timetrack_core::RuleError::LastProject.to_string()
        );
    }

    #[test]
    fn an_unknown_project_refuses_in_the_cores_words() {
        assert_eq!(
            delete_project_offer(&snap(), "nope").unwrap_err(),
            timetrack_core::RuleError::UnknownProject("nope".into()).to_string()
        );
    }

    #[test]
    fn the_banner_sanitizes_the_stored_name() {
        let snap = Snapshot {
            projects: vec![project("p1", "Work"), project("p2", "Em\npty\u{1b}[31m")],
            ..Default::default()
        };
        let offer = delete_project_offer(&snap, "p2").unwrap();
        assert!(
            !offer.summary.chars().any(|c| c.is_control()),
            "{}",
            offer.summary
        );
        assert!(offer.summary.contains("Em pty [31m"), "{}", offer.summary);
    }
}
