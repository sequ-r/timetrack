/* tray/menu.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The menu model: what one submenu per project shows.
//!
//! Pure over a snapshot — building the model touches no bus, no mutex,
//! and no ksni types — so the archived-filtering, overflow, and
//! last-entry rules are testable without a tray host. `item.rs` turns
//! the model into live `ksni` items.

use timetrack_proto::Snapshot;

/// How many projects sit at the menu's top level before the rest move under
/// "More projects". Tray menus must stay short; the aggregates behind them
/// still cover every project -- here it is the menu, not the data, that is
/// capped.
pub(crate) const MAX_PROJECTS: usize = 8;

/// One unarchived project as the menu sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectItem {
    pub(crate) id: String,
    pub(crate) name: String,
    /// This ISO week's total, from the service aggregate -- never re-summed
    /// from entries, which may carry only recent ones.
    pub(crate) week_ms: i64,
    /// The entry the delete row would remove: the most recent one for this
    /// project in the snapshot (tray-7, #11).
    pub(crate) last_entry: Option<LastEntry>,
}

/// The most recent entry for a project: its id plus its duration for the
/// confirm row. The snapshot arrives newest-first, so the first match per
/// project is the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LastEntry {
    pub(crate) id: String,
    pub(crate) duration_ms: i64,
}

/// What the menu shows. Compared by value each tick so the item only
/// rebuilds (and the host only re-renders) when something actually changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MenuModel {
    pub(crate) visible: Vec<ProjectItem>,
    pub(crate) overflow: Vec<ProjectItem>,
}

/// Build the menu model from a snapshot. Archived projects are filtered out
/// of the menu but stay in the totals (REQUIREMENTS §14); beyond
/// `MAX_PROJECTS` the remainder moves under an overflow submenu.
pub(crate) fn build_menu_model(snapshot: &Snapshot) -> MenuModel {
    let mut active: Vec<ProjectItem> = snapshot
        .projects
        .iter()
        .filter(|p| !p.archived)
        .map(|p| ProjectItem {
            id: p.id.clone(),
            // Sanitized once, here (#101): the model name becomes submenu
            // and confirm labels verbatim, so this is the one place that
            // keeps stored control characters out of every menu surface.
            name: timetrack_core::sanitize_display(&p.name),
            week_ms: snapshot.week.per_project.get(&p.id).copied().unwrap_or(0),
            last_entry: snapshot
                .entries
                .iter()
                .find(|e| e.project_id == p.id)
                .map(|e| LastEntry {
                    id: e.id.clone(),
                    duration_ms: e.duration_ms(),
                }),
        })
        .collect();
    let overflow = if active.len() > MAX_PROJECTS {
        active.split_off(MAX_PROJECTS)
    } else {
        Vec::new()
    };
    MenuModel {
        visible: active,
        overflow,
    }
}
