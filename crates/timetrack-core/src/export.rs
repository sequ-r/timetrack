/* core/export.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! CSV export (REQUIREMENTS §10).
//!
//! One row per entry, with a header row. Times are ISO 8601 local timestamps
//! (`2026-09-29T14:00:00`), not epoch millis — a stakeholder opens this in
//! Excel, not in a hex editor. Duration appears as both `HH:MM:SS` and raw
//! minutes, and `source` is included so hand-entered time is visible.
//!
//! Pure like the rest of the core: takes the store, a scope, `now` and the
//! UTC offset explicitly, so it is testable without I/O or a clock.

use crate::aggregate::{iso_week_of, month_of};
use crate::model::{MS_PER_DAY, Store, local_day_of};
use crate::{format_duration, local_day_start};

/// What an export covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportScope {
    /// The ISO week containing `now`.
    Week,
    /// The calendar month containing `now`.
    Month,
    /// Everything.
    #[default]
    All,
}

impl ExportScope {
    /// Parse the CLI / D-Bus spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "week" | "this-week" | "current-week" => Some(ExportScope::Week),
            "month" | "this-month" | "current-month" => Some(ExportScope::Month),
            "all" | "all-time" | "" => Some(ExportScope::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ExportScope::Week => "week",
            ExportScope::Month => "month",
            ExportScope::All => "all",
        }
    }
}

/// Header row, in field order.
pub const CSV_HEADER: &str =
    "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note";

/// Entries in `scope`, oldest first (the order a stakeholder reads).
pub fn entries_in_scope(
    store: &Store,
    scope: ExportScope,
    now_ms: i64,
    offset_ms: i64,
) -> Vec<&crate::Entry> {
    let mut out: Vec<&crate::Entry> = match scope {
        ExportScope::All => store.entries.iter().collect(),
        ExportScope::Week => {
            let today = local_day_of(now_ms, offset_ms);
            let week = iso_week_of(today);
            store
                .entries
                .iter()
                .filter(|e| {
                    let d = local_day_of(e.started_at, offset_ms);
                    d >= week.start_day && d < week.start_day + 7
                })
                .collect()
        }
        ExportScope::Month => {
            let today = local_day_of(now_ms, offset_ms);
            let (year, month) = month_of(today);
            store
                .entries
                .iter()
                .filter(|e| month_of(local_day_of(e.started_at, offset_ms)) == (year, month))
                .collect()
        }
    };
    out.sort_by_key(|e| e.started_at);
    out
}

/// Format a UTC instant as a local ISO 8601 timestamp without offset
/// (`2026-09-29T14:00:00`), per §10.
pub fn format_local_iso8601(utc_ms: i64, offset_ms: i64) -> String {
    let local_ms = utc_ms + offset_ms;
    let day = local_ms.div_euclid(MS_PER_DAY);
    let rem = local_ms.rem_euclid(MS_PER_DAY);
    let (y, m, d) = day_to_ymd(day);
    let (hh, mm, ss) = (
        rem / 3_600_000,
        (rem % 3_600_000) / 60_000,
        (rem % 60_000) / 1_000,
    );
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
}

/// Day index -> (year, month, day-of-month).
fn day_to_ymd(day: i64) -> (i64, u32, u32) {
    // Mirror aggregate.rs's walk so the two cannot disagree about month
    // lengths; this is the inverse of its day_of_month.
    let mut year = 1970;
    while days_to_year(year + 1) <= day && year < 10_000 {
        year += 1;
    }
    while days_to_year(year) > day && year > -10_000 {
        year -= 1;
    }
    let mut doy = day - days_to_year(year);
    let mut month = 1;
    loop {
        let len = days_in_month(year, month);
        if doy < len {
            break;
        }
        doy -= len;
        month += 1;
    }
    (year, month, (doy + 1) as u32)
}

fn days_to_year(year: i64) -> i64 {
    let mut days = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if is_leap(y) { 366 } else { 365 };
        }
    }
    days
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 30,
    }
}

/// Escape one CSV field (RFC 4180 minimal): quote when it contains
/// `,`, `"`, or a line break, doubling inner quotes.
pub fn escape_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render the store (or the scoped subset) as CSV, including the header.
pub fn export_csv(store: &Store, scope: ExportScope, now_ms: i64, offset_ms: i64) -> String {
    // Touch local_day_start so week/month bucketing stays anchored on the
    // same local-midnight definition the service uses.
    let _ = local_day_start(0, offset_ms);
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for e in entries_in_scope(store, scope, now_ms, offset_ms) {
        let project = store
            .project(&e.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or(e.project_id.as_str());
        let source = match e.source {
            crate::EntrySource::Manual => "manual",
            crate::EntrySource::QuickAdd => "quick_add",
        };
        let minutes = (e.duration_ms() / 60_000).to_string();
        let note = e.note.as_deref().unwrap_or("");
        let row = [
            escape_field(&e.id),
            escape_field(project),
            escape_field(&e.description),
            escape_field(&format_local_iso8601(e.started_at, offset_ms)),
            escape_field(&format_local_iso8601(e.ended_at, offset_ms)),
            escape_field(&format_duration(e.duration_ms())),
            minutes,
            source.to_string(),
            escape_field(note),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, EntrySource, Project};

    fn store() -> Store {
        let mut s = Store::default();
        s.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: None,
            archived: false,
        });
        s.entries.push(Entry {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "review".into(),
            started_at: 0,
            ended_at: 3_600_000,
            source: EntrySource::Manual,
            note: None,
        });
        s.entries.push(Entry {
            id: "e2".into(),
            project_id: "p1".into(),
            description: "a,b \"quoted\"".into(),
            started_at: 3_600_000,
            ended_at: 3_600_000 + 15 * 60_000,
            source: EntrySource::QuickAdd,
            note: Some("n".into()),
        });
        s
    }

    #[test]
    fn header_lists_the_spec_columns() {
        assert_eq!(
            CSV_HEADER,
            "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note"
        );
    }

    #[test]
    fn epoch_formats_as_midnight() {
        assert_eq!(format_local_iso8601(0, 0), "1970-01-01T00:00:00");
    }

    #[test]
    fn offset_shifts_the_wall_clock() {
        assert_eq!(format_local_iso8601(0, 3_600_000), "1970-01-01T01:00:00");
        assert_eq!(format_local_iso8601(0, -3_600_000), "1969-12-31T23:00:00");
    }

    #[test]
    fn known_date_formats() {
        // 2026-09-29T14:00:00Z as epoch millis.
        let utc = 1_790_690_400_000;
        assert_eq!(format_local_iso8601(utc, 0), "2026-09-29T14:00:00");
        assert_eq!(
            format_local_iso8601(utc, 2 * 3_600_000),
            "2026-09-29T16:00:00"
        );
    }

    #[test]
    fn day_round_trips_for_a_range() {
        for day in [-800, -1, 0, 1, 20000] {
            let (y, m, d) = day_to_ymd(day);
            assert!((1..=12).contains(&m), "{day} -> {y}-{m}-{d}");
            assert!((1..=31).contains(&d), "{day} -> {y}-{m}-{d}");
        }
        assert_eq!(day_to_ymd(0), (1970, 1, 1));
        assert_eq!(day_to_ymd(-1), (1969, 12, 31));
    }

    #[test]
    fn commas_and_quotes_are_escaped() {
        assert_eq!(escape_field("a,b"), "\"a,b\"");
        assert_eq!(escape_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(escape_field("plain"), "plain");
    }

    #[test]
    fn export_has_header_plus_one_row_per_entry() {
        let s = store();
        let csv = export_csv(&s, ExportScope::All, 0, 0);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], CSV_HEADER);
        assert!(lines[1].contains("review"));
        assert!(lines[1].contains("01:00:00"));
        assert!(lines[1].contains(",60,manual,"));
        assert!(lines[2].contains("quick_add"));
    }

    #[test]
    fn week_scope_excludes_other_weeks() {
        let s = store();
        // e1/e2 are in Jan 1970; "now" a fortnight later is a different week.
        let csv = export_csv(&s, ExportScope::Week, 14 * MS_PER_DAY, 0);
        assert_eq!(csv.lines().count(), 1, "header only");
    }

    #[test]
    fn scope_parses_cli_spellings() {
        assert_eq!(ExportScope::parse("week"), Some(ExportScope::Week));
        assert_eq!(ExportScope::parse("month"), Some(ExportScope::Month));
        assert_eq!(ExportScope::parse("all"), Some(ExportScope::All));
        assert_eq!(ExportScope::parse("everything"), None);
    }
}
