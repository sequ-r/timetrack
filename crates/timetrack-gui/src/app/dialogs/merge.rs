/* gui/app/dialogs/merge.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The merge confirmation: what would merge and how the total changes.
//!
//! Free functions over the snapshot, so the rule is testable without
//! constructing a GPUI view (which needs a live service connection).

use timetrack_proto::Snapshot;

/// Entries `m` would merge: the selected entry plus every same-project entry
/// it overlaps or touches.
///
/// Free over the snapshot like `newest_quick_add`, for the same reason.
/// Touching counts: joining end-to-end entries is the lossless case §8
/// exists for, and stopping at "overlap only" would refuse exactly that.
pub(crate) fn merge_targets(snapshot: &Snapshot, selected: usize) -> Vec<String> {
    let Some(picked) = snapshot.entries.get(selected) else {
        return Vec::new();
    };
    let mut ids = vec![picked.id.clone()];
    for e in &snapshot.entries {
        if e.id != picked.id
            && e.project_id == picked.project_id
            && e.started_at <= picked.ended_at
            && e.ended_at >= picked.started_at
        {
            ids.push(e.id.clone());
        }
    }
    if ids.len() < 2 { Vec::new() } else { ids }
}

/// A pending merge confirmation: the ids to send, and the one-line summary
/// the banner shows (count, project, and how the total changes).
#[derive(Debug, Clone)]
pub(crate) struct MergeConfirm {
    pub(crate) ids: Vec<String>,
    pub(crate) summary: String,
}

/// Build the merge confirmation for the selection, if there is anything to
/// merge. The delta comes from the shared core preview so the GUI can never
/// word it differently from the CLI.
pub(crate) fn merge_offer(snapshot: &Snapshot, selected: usize) -> Option<MergeConfirm> {
    let ids = merge_targets(snapshot, selected);
    if ids.len() < 2 {
        return None;
    }
    let by_id: std::collections::HashMap<&str, &timetrack_proto::EntryView> = snapshot
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let intervals: Vec<(i64, i64)> = ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()))
        .map(|e| (e.started_at, e.ended_at))
        .collect();
    let delta = timetrack_core::format_merge_delta(timetrack_core::merge_shrink_ms(&intervals));
    let project = by_id
        .get(ids[0].as_str())
        .and_then(|e| {
            snapshot
                .projects
                .iter()
                .find(|p| p.id == e.project_id)
                .map(|p| timetrack_core::sanitize_display(&p.name))
        })
        .unwrap_or_default();
    Some(MergeConfirm {
        summary: format!(
            "MERGE {} ENTRIES on {project}? {delta} — m to confirm, esc to cancel",
            ids.len()
        ),
        ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::{entry_on, merge_snapshot};

    #[test]
    fn merge_targets_are_the_overlapping_same_project_entries() {
        let snap = merge_snapshot();
        assert_eq!(
            merge_targets(&snap, 0),
            vec!["e1".to_string(), "e2".to_string()]
        );
        // The disjoint entry has nothing to merge with.
        assert!(merge_targets(&snap, 2).is_empty());
        // Nothing selected, nothing offered.
        assert!(merge_targets(&snap, 99).is_empty());
        assert!(merge_targets(&Snapshot::default(), 0).is_empty());
    }

    #[test]
    fn merge_targets_include_touching_entries() {
        let snap = Snapshot {
            entries: vec![entry_on("a", "p1", 0, 100), entry_on("b", "p1", 100, 200)],
            ..Default::default()
        };
        assert_eq!(
            merge_targets(&snap, 0),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn merge_offer_states_count_project_and_shrink() {
        let snap = merge_snapshot();
        let offer = merge_offer(&snap, 0).expect("e1 and e2 overlap");
        assert_eq!(offer.ids, vec!["e1".to_string(), "e2".to_string()]);
        assert!(
            offer.summary.contains("MERGE 2 ENTRIES"),
            "must state the count: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("Work"),
            "must name the project: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("total shrinks by 00:30:00"),
            "must state the shrink (§8): {}",
            offer.summary
        );
    }

    #[test]
    fn merge_offer_is_none_without_targets() {
        let snap = merge_snapshot();
        assert!(merge_offer(&snap, 2).is_none());
    }
}
