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
//! Pure like the rest of the core: takes the store, a scope, an anchor
//! instant and the UTC offset explicitly, so it is testable without I/O or a
//! clock.

use crate::aggregate::{iso_week_of, month_of};
use crate::format_duration;
use crate::model::{MS_PER_DAY, Store};
use crate::tz::Tz;

/// What an export covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportScope {
    /// The ISO week containing the anchor.
    Week,
    /// The calendar month containing the anchor.
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

    /// Stable identity of the period `anchor_ms` selects: the ISO week id,
    /// the month id, or `all time` (which has no id because its anchor is
    /// ignored). One table so the CLI, the GUI label and the saved status
    /// cannot disagree.
    pub fn period_id(self, anchor_ms: i64, tz: &Tz) -> String {
        match self {
            ExportScope::Week => week_id(anchor_ms, tz),
            ExportScope::Month => month_id(anchor_ms, tz),
            ExportScope::All => "all time".to_string(),
        }
    }

    /// Move the anchor one scope step per `delta`: seven days per week press,
    /// one calendar month per month press; all time is inert.
    ///
    /// Both week and month resolve through the day spelling
    /// ([`crate::parse_anchor_day`]), so a DST edge cannot move the anchor
    /// off midday: the week path re-midnights the shifted instant the same
    /// way the month path always has.
    pub fn shift(self, anchor_ms: i64, delta: i32, tz: &Tz) -> i64 {
        match self {
            ExportScope::All => anchor_ms,
            ExportScope::Week => {
                // Checked (#98): a hostile anchor/delta pair leaves the
                // anchor where it is instead of wrapping it across the epoch.
                let Some(shifted) = (delta as i64)
                    .checked_mul(7 * MS_PER_DAY)
                    .and_then(|d| anchor_ms.checked_add(d))
                else {
                    return anchor_ms;
                };
                let spelled = anchor_day_string(shifted, tz);
                crate::parse_anchor_day(&spelled, "anchor", tz).unwrap_or(shifted)
            }
            ExportScope::Month => shift_anchor_month(anchor_ms, delta, tz),
        }
    }
}

/// Whether one entry belongs in `scope` for `anchor_ms`.
///
/// The single bucketing predicate behind [`entries_in_scope_at`] and the GUI
/// preview: the week is the ISO week containing the anchor, the month the
/// calendar month containing it (both via `tz.day_of`); all time ignores the
/// anchor. Each entry buckets by its `started_at` only, never split or
/// duplicated (ADR-0002).
pub fn entry_in_scope(started_at_ms: i64, scope: ExportScope, anchor_ms: i64, tz: &Tz) -> bool {
    match scope {
        ExportScope::All => true,
        ExportScope::Week => {
            let week = iso_week_of(tz.day_of(anchor_ms));
            let d = tz.day_of(started_at_ms);
            d >= week.start_day && d < week.start_day + 7
        }
        ExportScope::Month => month_of(tz.day_of(started_at_ms)) == month_of(tz.day_of(anchor_ms)),
    }
}

/// The anchor's local day as `YYYY-MM-DD`, the spelling the typed field and
/// the CLI share. One helper so the GUI mirror and the week/month shifters
/// cannot disagree about the day boundary.
pub fn anchor_day_string(anchor_ms: i64, tz: &Tz) -> String {
    format_local_iso8601(anchor_ms, tz)
        .split('T')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Shift the anchor's calendar month, keeping the day where the month has
/// one. Resolves through the day spelling so a DST edge cannot move the
/// anchor off its day; clamps to the month length via the shared
/// [`crate::days_in_month`].
fn shift_anchor_month(anchor_ms: i64, delta: i32, tz: &Tz) -> i64 {
    let text = anchor_day_string(anchor_ms, tz);
    let mut parts = text.split('-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return anchor_ms;
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>()) else {
        return anchor_ms;
    };
    // `y`/`m` come from the clamped day spelling, so this cannot overflow
    // in practice; saturating keeps it that way by construction (#98).
    let total = y
        .saturating_mul(12)
        .saturating_add(m - 1)
        .saturating_add(delta as i64);
    let new_y = total.div_euclid(12);
    let new_m = total.rem_euclid(12) + 1;
    let new_d = d.min(crate::days_in_month(new_y, new_m as u32));
    let spelled = format!("{new_y:04}-{new_m:02}-{new_d:02}");
    crate::parse_anchor_day(&spelled, "anchor", tz).unwrap_or(anchor_ms)
}

/// Header row, in field order.
pub const CSV_HEADER: &str =
    "id,project,description,started_at,ended_at,duration_hms,duration_minutes,source,note";

/// Entries in `scope` for `anchor_ms`, oldest first (the order a stakeholder reads).
///
/// The week is the ISO week containing the anchor and the month is the
/// calendar month containing the anchor (both via `tz.day_of`); all time
/// ignores the anchor entirely. Each entry buckets by its `started_at` only,
/// never split or duplicated, so a 22:00-to-02:00 entry appears once with its
/// full duration in the start period.
pub fn entries_in_scope_at<'a>(
    store: &'a Store,
    scope: ExportScope,
    anchor_ms: i64,
    tz: &Tz,
) -> Vec<&'a crate::Entry> {
    let mut out: Vec<&crate::Entry> = match scope {
        ExportScope::All => store.entries.iter().collect(),
        _ => store
            .entries
            .iter()
            .filter(|e| entry_in_scope(e.started_at, scope, anchor_ms, tz))
            .collect(),
    };
    out.sort_by_key(|e| e.started_at);
    out
}

/// Entries in `scope` anchored at `now_ms` (the current week/month).
///
/// Backward-compat wrapper over [`entries_in_scope_at`]: existing callers
/// keep compiling and keep current-period behaviour, while new code paths
/// pass an explicit anchor.
pub fn entries_in_scope<'a>(
    store: &'a Store,
    scope: ExportScope,
    now_ms: i64,
    tz: &Tz,
) -> Vec<&'a crate::Entry> {
    entries_in_scope_at(store, scope, now_ms, tz)
}

/// Stable identity of the ISO week containing `anchor_ms` (`2026-W41`).
///
/// Monday-start per ISO 8601, resolved through `tz` so both UIs agree: the
/// same anchor on either side of a zone boundary names different weeks. The
/// year is the ISO year, so 2021-01-01 (a Friday) is `2020-W53` while
/// 2026-01-01 (a Thursday) opens `2026-W01`.
pub fn week_id(anchor_ms: i64, tz: &Tz) -> String {
    let week = iso_week_of(tz.day_of(anchor_ms));
    format!("{:04}-W{:02}", week.year, week.week)
}

/// Stable identity of the calendar month containing `anchor_ms` (`2026-09`).
///
/// The local month, not the ISO year: 2025-12-29 sits in ISO week `2026-W01`
/// but still in month `2025-12`.
pub fn month_id(anchor_ms: i64, tz: &Tz) -> String {
    let (year, month) = month_of(tz.day_of(anchor_ms));
    format!("{year:04}-{month:02}")
}

/// Default file name for saving an export of `scope` anchored at `anchor_ms`.
///
/// Week and month names carry their period id so one week never silently
/// overwrites the last; all time ignores the anchor and takes the plain name.
pub fn default_export_filename(scope: ExportScope, anchor_ms: i64, tz: &Tz) -> String {
    match scope {
        ExportScope::Week => format!("timetrack-export-week-{}.csv", week_id(anchor_ms, tz)),
        ExportScope::Month => format!("timetrack-export-month-{}.csv", month_id(anchor_ms, tz)),
        ExportScope::All => "timetrack-export-all.csv".to_string(),
    }
}

/// Status line after a saved export, naming entry count, scope, period, and file.
///
/// All time has no period (its anchor is ignored), so `period_id` is unused
/// there and the status reads `(all time)`.
pub fn format_export_status(
    entry_count: usize,
    scope: ExportScope,
    period_id: &str,
    basename: &str,
) -> String {
    match scope {
        ExportScope::All => format!("exported {entry_count} entries (all time) to {basename}"),
        _ => format!(
            "exported {entry_count} entries ({} {period_id}) to {basename}",
            scope.as_str()
        ),
    }
}

/// Format a UTC instant as a local ISO 8601 timestamp without offset
/// (`2026-09-29T14:00:00`), per §10.
///
/// Uses the offset in force at that instant, so entries on either side of a
/// DST transition render with their own wall-clock time.
pub fn format_local_iso8601(utc_ms: i64, tz: &Tz) -> String {
    // Saturating (#98): formatting an extreme stored instant renders the
    // range edge instead of wrapping to the opposite end of time.
    let local_ms = utc_ms.saturating_add(tz.offset_at_ms(utc_ms));
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
///
/// Delegates to the aggregate conversion ([`crate::ymd_of`]): there used to
/// be a second copy of the calendar walk here, with the same unbounded
/// month loop (#97). One conversion, clamped and constant-time.
fn day_to_ymd(day: i64) -> (i64, u32, u32) {
    crate::ymd_of(day)
}

/// Neutralize a spreadsheet formula hiding in a text cell (#101).
///
/// Excel, Sheets, and LibreOffice execute a cell whose first non-whitespace
/// character is `=`, `+`, `-`, or `@` as a formula — so a project named
/// `=2+2` (or worse, a DDE payload) would run in the stakeholder's
/// spreadsheet. Prefixing a single quote makes every major spreadsheet
/// treat the cell as text. RFC 4180 quoting alone does not stop this: the
/// quotes are CSV syntax, gone by the time the formula engine sees the cell.
fn neutralize_formula(s: &str) -> String {
    let first = s.trim_start_matches([' ', '\t', '\r']).chars().next();
    match first {
        Some('=' | '+' | '-' | '@') => format!("'{s}"),
        _ => s.to_string(),
    }
}

/// Escape one CSV field (RFC 4180 minimal): quote when it contains
/// `,`, `"`, or a line break, doubling inner quotes. Formula-looking text
/// is neutralized first (#101), quoting or not.
pub fn escape_field(s: &str) -> String {
    let s = neutralize_formula(s);
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// Render the store (or the scoped subset) as CSV, including the header.
///
/// `anchor_ms` selects which week or month `scope` covers; all time ignores
/// it. The CSV shape is frozen: header row, one row per entry in scope oldest
/// first, local wall-clock stamps without offset, readable duration plus
/// minutes, source column, minimal quoting.
pub fn export_csv_at(store: &Store, scope: ExportScope, anchor_ms: i64, tz: &Tz) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for e in entries_in_scope_at(store, scope, anchor_ms, tz) {
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
            escape_field(&format_local_iso8601(e.started_at, tz)),
            escape_field(&format_local_iso8601(e.ended_at, tz)),
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

/// Render the store (or the scoped subset) as CSV, anchored at `now_ms`.
///
/// Backward-compat wrapper over [`export_csv_at`]: existing callers keep
/// compiling and keep current-period behaviour, while new code paths pass an
/// explicit anchor.
pub fn export_csv(store: &Store, scope: ExportScope, now_ms: i64, tz: &Tz) -> String {
    export_csv_at(store, scope, now_ms, tz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, EntrySource, Project};

    fn utc() -> Tz {
        Tz::utc()
    }

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
        assert_eq!(format_local_iso8601(0, &utc()), "1970-01-01T00:00:00");
    }

    #[test]
    fn offset_shifts_the_wall_clock() {
        assert_eq!(
            format_local_iso8601(0, &Tz::fixed_ms(3_600_000)),
            "1970-01-01T01:00:00"
        );
        assert_eq!(
            format_local_iso8601(0, &Tz::fixed_ms(-3_600_000)),
            "1969-12-31T23:00:00"
        );
    }

    #[test]
    fn known_date_formats() {
        // 2026-09-29T14:00:00Z as epoch millis.
        let utc_ms = 1_790_690_400_000;
        assert_eq!(format_local_iso8601(utc_ms, &utc()), "2026-09-29T14:00:00");
        assert_eq!(
            format_local_iso8601(utc_ms, &Tz::fixed_ms(2 * 3_600_000)),
            "2026-09-29T16:00:00"
        );
    }

    #[test]
    fn named_zone_formats_with_its_own_offset_per_instant() {
        // Winter is +1, summer is +2 in Rome.
        let rome = Tz::parse("Europe/Rome").unwrap();
        assert_eq!(
            format_local_iso8601(1_768_478_400_000, &rome), // 2026-01-15 12:00 UTC
            "2026-01-15T13:00:00"
        );
        assert_eq!(
            format_local_iso8601(1_784_116_800_000, &rome), // 2026-07-15 12:00 UTC
            "2026-07-15T14:00:00"
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
    fn spreadsheet_formulas_are_neutralized() {
        // #101: a stakeholder opens this file in a spreadsheet; a cell
        // starting with =, +, -, @ (after whitespace) must arrive as text.
        assert_eq!(escape_field("=2+2"), "'=2+2");
        assert_eq!(escape_field("+2+2"), "'+2+2");
        assert_eq!(escape_field("-2+2"), "'-2+2");
        assert_eq!(escape_field("@SUM(1+1)"), "'@SUM(1+1)");
        assert_eq!(escape_field("  =2+2"), "'  =2+2");
        assert_eq!(
            escape_field("\t=HYPERLINK(\"x\")"),
            "\"'\t=HYPERLINK(\"\"x\"\")\""
        );
        // Ordinary text that merely contains the characters is untouched,
        // as are the generated (non-free-text) columns' shapes.
        assert_eq!(escape_field("a=b"), "a=b");
        assert_eq!(escape_field("C++"), "C++");
        assert_eq!(escape_field(""), "");
        assert_eq!(escape_field("2026-09-29T14:00:00"), "2026-09-29T14:00:00");
        // And it holds through a full export row, project and description.
        let mut s = store();
        s.projects[0].name = "=cmd|' /C calc'!A0".into();
        s.entries[0].description = "@2+2".into();
        let csv = export_csv(&s, ExportScope::All, 0, &utc());
        assert!(csv.contains("'=cmd|' /C calc'!A0"), "{csv}");
        assert!(csv.contains("'@2+2"), "{csv}");
        assert!(!csv.contains(",=cmd"), "no live formula cell: {csv}");
    }

    #[test]
    fn export_has_header_plus_one_row_per_entry() {
        let s = store();
        let csv = export_csv(&s, ExportScope::All, 0, &utc());
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
        let csv = export_csv(&s, ExportScope::Week, 14 * MS_PER_DAY, &utc());
        assert_eq!(csv.lines().count(), 1, "header only");
    }

    #[test]
    fn scope_parses_cli_spellings() {
        assert_eq!(ExportScope::parse("week"), Some(ExportScope::Week));
        assert_eq!(ExportScope::parse("month"), Some(ExportScope::Month));
        assert_eq!(ExportScope::parse("all"), Some(ExportScope::All));
        assert_eq!(ExportScope::parse("everything"), None);
    }

    #[test]
    fn week_id_formats_the_iso_week() {
        // 2026-10-08 is the Thursday of ISO week 41 (Mon 5 Oct - Sun 11 Oct).
        assert_eq!(week_id(1_791_460_800_000, &utc()), "2026-W41");
    }

    #[test]
    fn week_id_follows_the_iso_year_across_january_first() {
        // 2021-01-01 was a Friday, so it belongs to the last week of ISO
        // year 2020; 2026-01-01 was a Thursday, opening ISO year 2026.
        assert_eq!(week_id(1_609_502_400_000, &utc()), "2020-W53");
        assert_eq!(week_id(1_767_268_800_000, &utc()), "2026-W01");
        // ... and the Monday before it shares that week.
        assert_eq!(week_id(1_767_009_600_000, &utc()), "2026-W01");
    }

    #[test]
    fn week_id_uses_the_zone_offset() {
        // 2026-07-05 23:30 UTC is Sunday in UTC but Monday 00:30 at UTC+1,
        // so the two zones name different weeks for the same anchor.
        let anchor = 1_783_294_200_000;
        assert_eq!(week_id(anchor, &utc()), "2026-W27");
        assert_eq!(week_id(anchor, &Tz::fixed_ms(3_600_000)), "2026-W28");
    }

    #[test]
    fn month_id_formats_year_and_month() {
        assert_eq!(month_id(1_789_473_600_000, &utc()), "2026-09");
        assert_eq!(month_id(1_767_268_800_000, &utc()), "2026-01");
        // The local month is not the ISO year: 2025-12-29 sits in ISO week
        // 2026-W01 but still in calendar month 2025-12.
        assert_eq!(week_id(1_767_009_600_000, &utc()), "2026-W01");
        assert_eq!(month_id(1_767_009_600_000, &utc()), "2025-12");
    }

    #[test]
    fn default_filenames_carry_the_period() {
        assert_eq!(
            default_export_filename(ExportScope::Week, 1_791_460_800_000, &utc()),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            default_export_filename(ExportScope::Month, 1_789_473_600_000, &utc()),
            "timetrack-export-month-2026-09.csv"
        );
        assert_eq!(
            default_export_filename(ExportScope::All, 1_791_460_800_000, &utc()),
            "timetrack-export-all.csv"
        );
    }

    #[test]
    fn export_status_names_count_scope_period_and_file() {
        assert_eq!(
            format_export_status(
                12,
                ExportScope::Week,
                "2026-W41",
                "timetrack-export-week-2026-W41.csv"
            ),
            "exported 12 entries (week 2026-W41) to timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            format_export_status(
                3,
                ExportScope::Month,
                "2026-09",
                "timetrack-export-month-2026-09.csv"
            ),
            "exported 3 entries (month 2026-09) to timetrack-export-month-2026-09.csv"
        );
    }

    #[test]
    fn export_status_for_all_time_names_no_period() {
        // All time has no period (its anchor is ignored), so there is no
        // period id to name — the status says so plainly.
        assert_eq!(
            format_export_status(5, ExportScope::All, "all", "timetrack-export-all.csv"),
            "exported 5 entries (all time) to timetrack-export-all.csv"
        );
    }

    #[test]
    fn scope_period_id_names_week_month_or_all_time() {
        assert_eq!(
            ExportScope::Week.period_id(1_791_460_800_000, &utc()),
            "2026-W41"
        );
        assert_eq!(
            ExportScope::Month.period_id(1_789_473_600_000, &utc()),
            "2026-09"
        );
        assert_eq!(
            ExportScope::All.period_id(1_791_460_800_000, &utc()),
            "all time"
        );
    }

    #[test]
    fn scope_shift_moves_weeks_months_or_nothing() {
        let day = |text: &str| crate::parse_anchor_day(text, "anchor", &utc()).unwrap();
        assert_eq!(
            ExportScope::Week.shift(day("2026-01-01"), -1, &utc()),
            day("2025-12-25")
        );
        assert_eq!(
            ExportScope::Month.shift(day("2026-01-15"), -1, &utc()),
            day("2025-12-15")
        );
        assert_eq!(
            ExportScope::Month.shift(day("2026-03-31"), -1, &utc()),
            day("2026-02-28")
        );
        assert_eq!(
            ExportScope::All.shift(day("2026-09-15"), 1, &utc()),
            day("2026-09-15")
        );
    }

    #[test]
    fn scope_week_shift_stays_at_midday_across_dst() {
        // Spring forward 2026-03-29 (Rome): a fixed +7d in UTC would land at
        // 11:00 local; the shifter re-middays through the zone like months do.
        let rome = Tz::parse("Europe/Rome").unwrap();
        let before = crate::parse_anchor_day("2026-03-22", "anchor", &rome).unwrap();
        let after = ExportScope::Week.shift(before, 1, &rome);
        assert_eq!(anchor_day_string(after, &rome), "2026-03-29");
        assert_eq!(crate::format_local_hm(after, &rome), "12:00");
        // And back again across the same edge.
        let back = ExportScope::Week.shift(after, -1, &rome);
        assert_eq!(anchor_day_string(back, &rome), "2026-03-22");
        assert_eq!(crate::format_local_hm(back, &rome), "12:00");
        // Fall back 2026-10-25: same pin the other way.
        let before = crate::parse_anchor_day("2026-10-18", "anchor", &rome).unwrap();
        let after = ExportScope::Week.shift(before, 1, &rome);
        assert_eq!(anchor_day_string(after, &rome), "2026-10-25");
        assert_eq!(crate::format_local_hm(after, &rome), "12:00");
    }

    #[test]
    fn entry_predicate_matches_the_scoped_store_query() {
        // The GUI preview filters snapshots with this; the store query must
        // agree exactly, or the preview lies about the file.
        let s = anchored_store(&[
            ("e_mon", 1_791_194_400_000, 1_791_198_000_000),
            ("e_next", 1_791_799_200_000, 1_791_802_800_000),
        ]);
        let anchor = 1_791_460_800_000;
        let via_store: Vec<&str> = entries_in_scope_at(&s, ExportScope::Week, anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let via_predicate: Vec<&str> = s
            .entries
            .iter()
            .filter(|e| entry_in_scope(e.started_at, ExportScope::Week, anchor, &utc()))
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(via_store, via_predicate);
        assert_eq!(via_store, ["e_mon"]);
    }

    fn anchored_store(rows: &[(&str, i64, i64)]) -> Store {
        let mut s = Store::default();
        s.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: None,
            archived: false,
        });
        for (id, a, b) in rows {
            s.entries.push(Entry {
                id: (*id).into(),
                project_id: "p1".into(),
                description: (*id).into(),
                started_at: *a,
                ended_at: *b,
                source: EntrySource::Manual,
                note: None,
            });
        }
        s
    }

    #[test]
    fn chosen_week_returns_exact_starts_in_week_set() {
        // ISO week 41 of 2026 runs Mon 5 Oct - Sun 11 Oct (UTC).
        let s = anchored_store(&[
            ("e_mon", 1_791_194_400_000, 1_791_198_000_000), // Mon 5 Oct 10:00-11:00
            ("e_sun", 1_791_712_800_000, 1_791_716_400_000), // Sun 11 Oct 10:00-11:00
            ("e_next", 1_791_799_200_000, 1_791_802_800_000), // Mon 12 Oct 10:00-11:00
        ]);
        let anchor = 1_791_460_800_000; // Thu 8 Oct 12:00 UTC, inside W41
        let ids: Vec<&str> = entries_in_scope_at(&s, ExportScope::Week, anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["e_mon", "e_sun"]);
        let csv = export_csv_at(&s, ExportScope::Week, anchor, &utc());
        assert!(
            csv.contains("e_mon") && csv.contains("e_sun"),
            "got:\n{csv}"
        );
        assert!(
            !csv.contains("e_next"),
            "next week must not leak in:\n{csv}"
        );
    }

    #[test]
    fn chosen_month_returns_exact_starts_in_month_set() {
        let s = anchored_store(&[
            ("e_sep", 1_789_466_400_000, 1_789_470_000_000), // 15 Sep 10:00-11:00
            ("e_sep_end", 1_790_762_400_000, 1_790_766_000_000), // 30 Sep 10:00-11:00
            ("e_oct", 1_790_848_800_000, 1_790_852_400_000), // 1 Oct 10:00-11:00
        ]);
        let sep_anchor = 1_789_473_600_000; // 15 Sep 12:00 UTC
        let oct_anchor = 1_791_460_800_000; // 8 Oct 12:00 UTC
        let sep_ids: Vec<&str> = entries_in_scope_at(&s, ExportScope::Month, sep_anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(sep_ids, ["e_sep", "e_sep_end"]);
        let oct_ids: Vec<&str> = entries_in_scope_at(&s, ExportScope::Month, oct_anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(oct_ids, ["e_oct"]);
        let csv = export_csv_at(&s, ExportScope::Month, sep_anchor, &utc());
        assert!(
            !csv.contains("e_oct"),
            "October must not leak into September:\n{csv}"
        );
    }

    #[test]
    fn all_ignores_anchor_entirely() {
        let s = anchored_store(&[
            ("e_sep", 1_789_466_400_000, 1_789_470_000_000),
            ("e_oct", 1_790_848_800_000, 1_790_852_400_000),
        ]);
        let sep_anchor = 1_789_473_600_000;
        let oct_anchor = 1_791_460_800_000;
        let a: Vec<&str> = entries_in_scope_at(&s, ExportScope::All, sep_anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let b: Vec<&str> = entries_in_scope_at(&s, ExportScope::All, oct_anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(a, ["e_sep", "e_oct"]);
        assert_eq!(
            b,
            ["e_sep", "e_oct"],
            "a stale anchor must never narrow a full dump"
        );
        assert_eq!(
            export_csv_at(&s, ExportScope::All, sep_anchor, &utc()),
            export_csv_at(&s, ExportScope::All, oct_anchor, &utc()),
            "all-time bytes must not depend on the anchor"
        );
    }

    #[test]
    fn spanning_entry_appears_once_with_full_duration_in_start_week() {
        // Sun 11 Oct 22:00 UTC -> Mon 12 Oct 02:00 UTC: 4h across the W41/W42 line.
        let s = anchored_store(&[("e_span", 1_791_756_000_000, 1_791_770_400_000)]);
        let w41 = 1_791_460_800_000; // Thu 8 Oct 12:00 UTC
        let w42 = 1_791_806_400_000; // Mon 12 Oct 12:00 UTC
        let in_start: Vec<&str> = entries_in_scope_at(&s, ExportScope::Week, w41, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(in_start, ["e_span"]);
        assert!(
            entries_in_scope_at(&s, ExportScope::Week, w42, &utc()).is_empty(),
            "the end week must not repeat the entry"
        );
        let csv = export_csv_at(&s, ExportScope::Week, w41, &utc());
        assert_eq!(
            csv.lines().count(),
            2,
            "header plus one row, never duplicated:\n{csv}"
        );
        assert!(
            csv.contains("04:00:00") && csv.contains(",240,"),
            "full duration, got:\n{csv}"
        );
        assert_eq!(
            export_csv_at(&s, ExportScope::Week, w42, &utc())
                .lines()
                .count(),
            1,
            "header only in the end week"
        );
    }

    #[test]
    fn spanning_entry_appears_once_with_full_duration_in_start_month() {
        // 30 Sep 22:00 UTC -> 1 Oct 02:00 UTC: 4h across the Sep/Oct line.
        let s = anchored_store(&[("e_span", 1_790_805_600_000, 1_790_820_000_000)]);
        let sep_anchor = 1_789_473_600_000;
        let oct_anchor = 1_791_460_800_000;
        assert_eq!(
            entries_in_scope_at(&s, ExportScope::Month, sep_anchor, &utc()).len(),
            1
        );
        assert!(
            entries_in_scope_at(&s, ExportScope::Month, oct_anchor, &utc()).is_empty(),
            "October must not repeat the September entry"
        );
        let csv = export_csv_at(&s, ExportScope::Month, sep_anchor, &utc());
        assert_eq!(csv.lines().count(), 2, "header plus one row:\n{csv}");
        assert!(
            csv.contains("04:00:00") && csv.contains(",240,"),
            "full duration, got:\n{csv}"
        );
    }

    #[test]
    fn overlapping_entries_appear_as_separate_rows_oldest_first() {
        // Inserted newest-first on purpose: the export must still read oldest first.
        let s = anchored_store(&[
            ("e_late", 1_791_198_000_000, 1_791_205_200_000), // Mon 5 Oct 11:00-13:00
            ("e_early", 1_791_194_400_000, 1_791_201_600_000), // Mon 5 Oct 10:00-12:00
        ]);
        let anchor = 1_791_460_800_000;
        let ids: Vec<&str> = entries_in_scope_at(&s, ExportScope::Week, anchor, &utc())
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, ["e_early", "e_late"]);
        let csv = export_csv_at(&s, ExportScope::Week, anchor, &utc());
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3, "header plus one row per entry:\n{csv}");
        assert!(lines[1].contains("e_early"), "oldest first, got:\n{csv}");
        assert!(lines[2].contains("e_late"), "got:\n{csv}");
    }
}
