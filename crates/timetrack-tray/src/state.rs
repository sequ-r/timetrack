/* tray/state.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Snapshot-derived display state: the tooltip summary and the
//! connection-health classifications.
//!
//! Pure functions over poll outcomes, so the down/skewed/refused states
//! are drivable in tests without a bus. Sorting is always by
//! `ClientError` variant, never by message text.

use timetrack_proto::{ClientError, Snapshot};

/// What the tooltip and icon state show. Replaced wholesale from the poll
/// loop via `Handle::update`, like the menu -- and part of the same rebuild
/// gate, so the host re-renders on any real change, not every tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TooltipState {
    /// This ISO week's total, from the service aggregate.
    pub(crate) total_ms: i64,
    /// How many projects share that total (nonzero week total each).
    pub(crate) projects: usize,
    /// A day's summed time exceeds 24h (REQUIREMENTS §4): the icon asks for
    /// attention instead of sitting idle.
    pub(crate) attention: bool,
}

impl TooltipState {
    /// Derive the tooltip state from a snapshot: the week total, the
    /// contributor count through `Snapshot::project_totals`, and the
    /// over-24h flag. Entries are never re-summed -- they may carry only
    /// recent ones while the aggregates cover the whole store.
    pub(crate) fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            total_ms: snapshot.week.total_ms,
            projects: snapshot
                .projects
                .iter()
                .filter(|p| snapshot.project_totals(&p.id).0 > 0)
                .count(),
            attention: !snapshot.over_24h_days.is_empty(),
        }
    }
}

/// The tooltip body: the week total plus how many projects share it.
///
/// The total is formatted like the menu rows beside it -- the core's
/// compact `1h 15m`. The tooltip used to have a formatter of its own
/// (`1:15`), so the tray's two surfaces spelled the same duration two
/// ways, and the one that was not in the core was the one that was not
/// shared.
pub(crate) fn tooltip_text(state: &TooltipState) -> String {
    let projects = if state.projects == 1 {
        "project"
    } else {
        "projects"
    };
    format!(
        "Week total {} · {} {projects}",
        timetrack_core::format_compact(state.total_ms),
        state.projects
    )
}

/// Why the icon is not showing live totals: the last poll outcome when it
/// was not a good snapshot. `None` means the tooltip shows the week summary
/// above. Sorted by `ClientError` variant (see `unwell_of`), never by
/// message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unwell {
    /// The service is unreachable (absent, or the bus broke mid-run): the
    /// icon goes flat and the tooltip carries the start-service hint.
    Down,
    /// Client and service speak different interfaces: a distinct message
    /// naming the reinstall fix.
    Skewed,
    /// The service refused the snapshot: its own wording, like the GUI's
    /// `StatusKind::Service`.
    Refused(String),
}

/// Map a poll outcome to the icon's unwell state, if any. Pure over the
/// result so both down states are drivable without a bus (tray-9, #13).
pub(crate) fn unwell_of(result: &Result<Snapshot, ClientError>) -> Option<Unwell> {
    match result {
        Ok(_) => None,
        Err(ClientError::NoService) | Err(ClientError::Transport(_)) => Some(Unwell::Down),
        Err(ClientError::VersionMismatch { .. }) => Some(Unwell::Skewed),
        Err(ClientError::Service(e)) => Some(Unwell::Refused(e.to_string())),
    }
}

/// Where the poll loop stands. Sorts by `ClientError` variant, reusing the
/// triage in `timetrack-proto` -- never by message text, so a refusal that
/// merely mentions the bus can never read as a missing service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    Ready,
    NoService,
    VersionMismatch,
    Refused,
    Transport,
}

pub(crate) fn conn_state(result: &Result<Snapshot, ClientError>) -> ConnState {
    match result {
        Ok(_) => ConnState::Ready,
        Err(ClientError::NoService) => ConnState::NoService,
        Err(ClientError::VersionMismatch { .. }) => ConnState::VersionMismatch,
        Err(ClientError::Service(_)) => ConnState::Refused,
        Err(ClientError::Transport(_)) => ConnState::Transport,
    }
}
