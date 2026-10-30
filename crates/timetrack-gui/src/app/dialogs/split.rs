/* gui/app/dialogs/split.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The split prompt: one moment field for the selected entry.
//!
//! Keystroke editing like the entry dialog (caret and selection included,
//! to delete, enter to split, escape to cancel), and likewise pure state
//! over strings so the whole interaction is unit-testable without a window.

/// The split prompt: one moment field for the selected entry.
///
/// Keystroke editing like the entry dialog (caret and selection included,
/// to delete, enter to split, escape to cancel), and likewise pure state
/// over strings so the whole interaction is unit-testable without a window.
#[derive(Debug, Clone)]
pub(crate) struct SplitPrompt {
    /// The entry being split.
    pub(crate) entry_id: String,
    /// Its interval, for the title and the range pre-check.
    pub(crate) started_at: i64,
    pub(crate) ended_at: i64,
    /// The typed split point (`-30`, `09:30`, `now` -- the shared spellings).
    pub(crate) at: String,
    /// The caret (and any selection) in the moment field (#109).
    pub(crate) caret: crate::app::textline::Caret,
    pub(crate) error: Option<String>,
}

/// Resolve a split prompt to the split instant.
///
/// Pure over the prompt plus an explicit clock and zone, like `resolve_save`,
/// so parsing, the range pre-check and the empty-field refusal are all
/// testable without a view or a service. The service re-validates the range
/// on execute; this pre-check only phrases the refusal for the typed moment.
pub(crate) fn resolve_split(
    prompt: &SplitPrompt,
    now: i64,
    tz: &timetrack_core::Tz,
) -> Result<i64, String> {
    if prompt.at.trim().is_empty() {
        return Err("enter a split point, e.g. -30 or 09:30".to_string());
    }
    let at_ms =
        timetrack_core::parse_moment(&prompt.at, "at", now, tz).map_err(|e| e.to_string())?;
    if at_ms <= prompt.started_at || at_ms >= prompt.ended_at {
        return Err(format!(
            "split point is outside entry '{}'",
            prompt.entry_id
        ));
    }
    Ok(at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::split_prompt;

    #[test]
    fn split_resolves_an_interior_moment() {
        let utc = timetrack_core::Tz::utc();
        // now = 00:45 on day 0; -15 lands at 00:30, inside [0h, 1h].
        let now = 45 * 60_000;
        assert_eq!(
            resolve_split(&split_prompt("-15"), now, &utc),
            Ok(30 * 60_000)
        );
    }

    #[test]
    fn split_refuses_empty_garbage_and_outside_points() {
        let utc = timetrack_core::Tz::utc();
        let now = 45 * 60_000;
        assert!(resolve_split(&split_prompt(""), now, &utc).is_err());
        assert!(resolve_split(&split_prompt("soon"), now, &utc).is_err());
        // Endpoints are not interior: the service would refuse these too.
        assert!(resolve_split(&split_prompt("00:00"), now, &utc).is_err());
        let err = resolve_split(&split_prompt("02:00"), now, &utc).unwrap_err();
        assert!(
            err.contains("outside entry 'e1'"),
            "must name the entry: {err}"
        );
    }
}
