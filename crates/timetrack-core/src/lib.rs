/* core/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! TimeTrack's domain core.
//!
//! The data model, the rules that can change it, and the aggregation that
//! turns entries into weekly and monthly totals (REQUIREMENTS §3, §4, §8, §9).
//!
//! # What this crate deliberately does not depend on
//!
//! No GUI, no IPC, no async, and **no clock**. Every function that needs the
//! current time takes it as a parameter. That is what makes the rules
//! testable and the aggregation reproducible, and it is why the core can be
//! shared unchanged by the service, the desktop GUI and the terminal UI.
//!
//! # Storage
//!
//! `storage` is the one module that touches a filesystem. It is kept out of
//! `rules` so the rules stay pure, and out of the clients' dependency path so
//! the CLI never links it.
//!
//! # Timezone
//!
//! Local-time arithmetic (which day or week an instant falls in) takes an
//! explicit [`Tz`] rather than reading a clock or the environment. The service
//! resolves the zone once from `TZ` (or the system zone); every instant below
//! this line is then mapped through the offset in force *at that instant*,
//! so DST transitions bucket correctly. This is the app's
//! likeliest source of off-by-one bugs, so it is deliberately concentrated
//! in `tz.rs` plus the thin call sites that pass a `&Tz` through.

pub mod aggregate;
pub mod export;
pub mod model;
pub mod parse;
pub mod rules;
pub mod storage;
pub mod tz;

pub use aggregate::{
    IsoWeek, SnapshotTotals, Totals, all_totals, days_in_month, is_leap_year, iso_week_of,
    month_of, month_totals, snapshot_totals, totals_in_day_range, week_totals, ymd_of,
};
pub use export::{
    CSV_HEADER, ExportScope, anchor_day_string, default_export_filename, entries_in_scope,
    entries_in_scope_at, entry_in_scope, escape_field, export_csv, export_csv_at,
    format_export_status, format_local_iso8601, month_id, week_id,
};
pub use model::{
    Entry, EntrySource, MAX_DAY, MAX_INSTANT_MS, MIN_DAY, MIN_INSTANT_MS, MS_PER_DAY, Project,
    STORE_VERSION, Store, instant_in_range, local_day_of, local_day_start,
};
pub use parse::{
    ParseError, ParseResult, format_local_hm, parse_anchor_day, parse_colour, parse_duration,
    parse_moment,
};
pub use rules::{
    Counter, Ids, MAX_DESCRIPTION_CHARS, MAX_NOTE_CHARS, MAX_PROJECT_NAME_CHARS, QUICK_ADD_MS,
    RuleError, RuleResult, create_duration, create_duration_ending, create_entry, create_project,
    days_exceeding_24h, delete_entry, delete_project, format_merge_delta, merge_entries,
    merge_shrink_ms, next_project_id, quick_add, set_archived, set_project, set_text, set_times,
    split_entry, update_project,
};
pub use storage::{JsonStore, StorageError, StorageResult};
pub use tz::Tz;

/// A single-line display copy of stored free text (#101).
///
/// Project names and descriptions are user text that renderers show in
/// terminals, menus, and tooltips. The rules refuse control characters in
/// new text, but a store written before that check (or edited by hand) can
/// still hold newlines and escape sequences that would break terminal
/// columns or drive the terminal itself. Every renderer therefore displays
/// this copy — control characters replaced by spaces — and never the raw
/// stored bytes. The stored value is untouched; this is presentation only.
pub fn sanitize_display(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Format a duration in milliseconds as `HH:MM:SS`.
///
/// Hours are not wrapped at 24, deliberately: a day whose summed time exceeds
/// 24h is legal (§4) and should read as such rather than silently wrapping.
pub fn format_duration(ms: i64) -> String {
    let total = (ms / 1000).max(0);
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// Format a duration in milliseconds as a compact human string, e.g. `2h 15m`.
///
/// For dense lists where `HH:MM:SS` is too wide to be readable.
pub fn format_compact(ms: i64) -> String {
    let mins = (ms / 60_000).max(0);
    let (h, m) = (mins / 60, mins % 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_with_padding() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(1_000), "00:00:01");
        assert_eq!(format_duration(61_000), "00:01:01");
        assert_eq!(format_duration(3_600_000), "01:00:00");
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        assert_eq!(format_duration(-5_000), "00:00:00");
    }

    #[test]
    fn over_24h_does_not_wrap() {
        // A day may legitimately sum past 24h (§4); wrapping would hide the
        // very thing the 24h warning exists to surface.
        assert_eq!(format_duration(30 * 3_600_000), "30:00:00");
    }

    #[test]
    fn display_copies_carry_no_control_characters() {
        // #101: newlines, ANSI escapes, and other C0/C1 controls become
        // spaces; ordinary text (including non-ASCII) passes through.
        assert_eq!(sanitize_display("Work"), "Work");
        assert_eq!(sanitize_display("a\nb\rc\td"), "a b c d");
        assert_eq!(sanitize_display("\u{1b}[31mred\u{1b}[0m"), " [31mred [0m");
        assert_eq!(sanitize_display("café ☕"), "café ☕");
        assert_eq!(sanitize_display(""), "");
        assert!(
            !sanitize_display("x\u{7}y\u{85}z")
                .chars()
                .any(|c| c.is_control()),
            "no control character survives"
        );
    }

    #[test]
    fn compact_format_is_readable() {
        assert_eq!(format_compact(0), "0m");
        assert_eq!(format_compact(5 * 60_000), "5m");
        assert_eq!(format_compact(60 * 60_000), "1h");
        assert_eq!(format_compact(2 * 3_600_000 + 15 * 60_000), "2h 15m");
    }
}
