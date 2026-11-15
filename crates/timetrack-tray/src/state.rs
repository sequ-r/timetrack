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
pub struct TooltipState {
    /// This ISO week's total, from the service aggregate.
    pub total_ms: i64,
    /// How many projects share that total (nonzero week total each).
    pub projects: usize,
    /// A day's summed time exceeds 24h (REQUIREMENTS §4): the icon asks for
    /// attention instead of sitting idle.
    pub attention: bool,
}

impl TooltipState {
    /// Derive the tooltip state from a snapshot: the week total, the
    /// contributor count through `Snapshot::project_totals`, and the
    /// over-24h flag. Entries are never re-summed -- they may carry only
    /// recent ones while the aggregates cover the whole store.
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
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
pub fn tooltip_text(state: &TooltipState) -> String {
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
pub enum Unwell {
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
pub fn unwell_of(result: &Result<Snapshot, ClientError>) -> Option<Unwell> {
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
pub enum ConnState {
    Ready,
    NoService,
    VersionMismatch,
    Refused,
    Transport,
}

/// What the user is told when the tray has nowhere to appear (#105).
///
/// The tray keeps running — a watcher can appear later, and ksni
/// re-registers when it does — but running *silently* invisible would
/// read exactly like quick-add being broken. This is the notification
/// (and log line) that says where quick-add lives on such a desktop
/// instead, in the same terms the README uses.
pub fn no_watcher_notice() -> (&'static str, &'static str) {
    (
        "The TimeTrack tray cannot appear on this desktop",
        "No status-notifier watcher is running. Quick-add instead: right-click the TimeTrack launcher for +5/+15/+30/+60, use keys 1-4 in the app, or run `timetrack quick -m 15`. For the tray on GNOME, install an AppIndicator extension.",
    )
}

pub fn conn_state(result: &Result<Snapshot, ClientError>) -> ConnState {
    match result {
        Ok(_) => ConnState::Ready,
        Err(ClientError::NoService) => ConnState::NoService,
        Err(ClientError::VersionMismatch { .. }) => ConnState::VersionMismatch,
        Err(ClientError::Service(_)) => ConnState::Refused,
        Err(ClientError::Transport(_)) => ConnState::Transport,
    }
}

/// The icon state every backend renders: passive while the service is
/// down, attention while something needs the user, active otherwise.
/// Mirrors `ksni::Status` without naming it, so the mapping stays
/// testable on targets without ksni.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeutralStatus {
    Passive,
    Active,
    NeedsAttention,
}

/// The icon state plus the tooltip body, pure over the three inputs the
/// backends hold: the week summary, the connection health, and the last
/// action failure.
///
/// Priority, lifted verbatim from the Linux backend: a missing service
/// flattens the icon; a skew, a refused snapshot, a failed action or an
/// over-24h day asks for attention; otherwise the icon sits idle-active.
/// The tooltip shows the connection problem first (it explains more than
/// the refusal that failure caused), then the action failure (it is what
/// the user just did), then the week summary. Service-origin text is
/// sanitized (#101); the shared hints pass through.
pub fn presentation(
    summary: &TooltipState,
    unwell: Option<&Unwell>,
    action_error: Option<&str>,
) -> (NeutralStatus, String) {
    // Attention travels via status, not a second icon: only one icon
    // asset is installed, so a second icon name would render nothing.
    let status = match unwell {
        Some(Unwell::Down) => NeutralStatus::Passive,
        Some(Unwell::Skewed) => NeutralStatus::NeedsAttention,
        Some(Unwell::Refused(_)) | None => {
            if summary.attention || action_error.is_some() {
                NeutralStatus::NeedsAttention
            } else {
                NeutralStatus::Active
            }
        }
    };
    let description = match unwell {
        Some(Unwell::Down) => timetrack_proto::no_service_hint(),
        Some(Unwell::Skewed) => timetrack_proto::version_mismatch_hint("tray"),
        Some(Unwell::Refused(text)) => timetrack_core::sanitize_display(text),
        None => match action_error {
            Some(text) => timetrack_core::sanitize_display(text),
            None => tooltip_text(summary),
        },
    };
    (status, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(total_ms: i64, projects: usize, attention: bool) -> TooltipState {
        TooltipState {
            total_ms,
            projects,
            attention,
        }
    }

    #[test]
    fn healthy_week_sits_idle_active() {
        let (status, tip) = presentation(&summary(90 * 60_000, 2, false), None, None);
        assert_eq!(status, NeutralStatus::Active);
        assert_eq!(tip, "Week total 1h 30m · 2 projects");
    }

    #[test]
    fn over_24h_day_asks_for_attention_but_keeps_the_total() {
        let (status, tip) = presentation(&summary(25 * 3_600_000, 1, true), None, None);
        assert_eq!(status, NeutralStatus::NeedsAttention);
        assert_eq!(tip, "Week total 25h · 1 project");
    }

    #[test]
    fn down_flattens_the_icon_with_the_start_hint() {
        let (status, tip) =
            presentation(&summary(90 * 60_000, 2, false), Some(&Unwell::Down), None);
        assert_eq!(status, NeutralStatus::Passive);
        assert_eq!(tip, timetrack_proto::no_service_hint());
    }

    #[test]
    fn down_outranks_attention_and_action_failures() {
        // A connection problem explains more than the failure it caused.
        let (status, tip) = presentation(
            &summary(0, 0, true),
            Some(&Unwell::Down),
            Some("the timetrack service is not running"),
        );
        assert_eq!(status, NeutralStatus::Passive);
        assert_eq!(tip, timetrack_proto::no_service_hint());
    }

    #[test]
    fn skew_has_its_own_message_and_attention() {
        let (status, tip) = presentation(&summary(0, 0, false), Some(&Unwell::Skewed), None);
        assert_eq!(status, NeutralStatus::NeedsAttention);
        assert_eq!(tip, timetrack_proto::version_mismatch_hint("tray"));
    }

    #[test]
    fn refusal_shows_the_services_own_wording_and_follows_attention() {
        let (status, tip) = presentation(
            &summary(0, 0, false),
            Some(&Unwell::Refused("older service".into())),
            None,
        );
        assert_eq!(status, NeutralStatus::Active);
        assert_eq!(tip, "older service");
        let (status, _) = presentation(
            &summary(0, 0, true),
            Some(&Unwell::Refused("older service".into())),
            None,
        );
        assert_eq!(status, NeutralStatus::NeedsAttention);
    }

    #[test]
    fn refusal_outranks_a_failed_action() {
        let (status, tip) = presentation(
            &summary(0, 0, false),
            Some(&Unwell::Refused("refused snapshot".into())),
            Some("quick add failed"),
        );
        assert_eq!(tip, "refused snapshot");
        assert_eq!(status, NeutralStatus::NeedsAttention);
    }

    #[test]
    fn failed_action_outranks_the_week_summary() {
        let (status, tip) = presentation(
            &summary(90 * 60_000, 2, false),
            None,
            Some("the timetrack service is not running"),
        );
        assert_eq!(status, NeutralStatus::NeedsAttention);
        assert_eq!(tip, "the timetrack service is not running");
    }

    #[test]
    fn service_origin_text_is_sanitized() {
        // #101: a refusal can embed stored text.
        let (status, tip) = presentation(
            &summary(0, 0, false),
            Some(&Unwell::Refused("bad\n\x1b[31m".into())),
            None,
        );
        assert_eq!(status, NeutralStatus::Active);
        assert_eq!(tip, "bad  [31m");
        assert!(!tip.chars().any(|c| c.is_control()));
        let (_, tip) = presentation(&summary(0, 0, false), None, Some("bad\ntext"));
        assert_eq!(tip, "bad text");
    }
}
