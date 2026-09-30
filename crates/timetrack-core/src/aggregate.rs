/* core/aggregate.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Weekly and monthly totals (REQUIREMENTS §9).
//!
//! All aggregation lives here, in the core, so the service has one
//! implementation and both UIs cannot disagree about where a week starts.
//!
//! Totals are **sums of durations**, never wall-clock unions (§4), so
//! overlapping entries are counted separately.

use crate::model::{Store, local_day_of};
use std::collections::BTreeMap;

/// A calendar week, as ISO-8601 defines it: Monday 00:00 to Sunday 23:59.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoWeek {
    pub year: i64,
    /// 1..=53, ISO week number.
    pub week: u64,
    /// Local day index of that week's Monday.
    pub start_day: i64,
}

/// Totals for one period, overall and per project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    /// Sum of all durations in the period.
    pub total_ms: i64,
    /// Per project, ordered by id for a stable display order.
    pub per_project: BTreeMap<String, i64>,
    /// Number of entries contributing.
    pub entry_count: usize,
}

impl Totals {
    /// Projects ordered by time spent, longest first. Ties break on id so the
    /// order never flickers between runs.
    pub fn ranked(&self) -> Vec<(&String, i64)> {
        let mut v: Vec<(&String, i64)> = self.per_project.iter().map(|(k, v)| (k, *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v
    }
}

/// The ISO week containing local day `day`.
///
/// ISO 8601, which is what REQUIREMENTS §9 specifies: weeks start on Monday,
/// and week 1 is the week containing the first Thursday of the year. That last
/// rule is why this is not just "Monday of this week" — it is what makes the
/// week-number stable across the new year.
pub fn iso_week_of(day: i64) -> IsoWeek {
    // ISO weekday: Monday = 1 .. Sunday = 7.
    // 1970-01-01 was a Thursday, which is ISO day 4.
    let iso_weekday = (day + 3).rem_euclid(7) + 1;
    let monday = day - (iso_weekday - 1);

    // The Thursday of this week decides the ISO year.
    let thursday = monday + 3;

    // Bounded upward walk: `days_to_year` is monotonic, so this converges, and
    // the clamp keeps it finite for pre-epoch days.
    let mut year = 1970;
    while days_to_year(year + 1) <= thursday && year < 10_000 {
        year += 1;
    }
    // ISO week 1 is the week containing the first Thursday, so the Thursday
    // of week 1 falls between Jan 1 and Jan 7.
    let jan1 = days_to_year(year);
    let week = (thursday - jan1).div_euclid(7) + 1;
    IsoWeek {
        year,
        week: week as u64,
        start_day: monday,
    }
}

/// Days from the epoch to 1 January `year`. Walks year by year so leap days
/// are counted exactly.
fn days_to_year(year: i64) -> i64 {
    let mut days = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    days
}

/// The calendar month containing local day `day`, as `(year, month)`.
pub fn month_of(day: i64) -> (i64, u32) {
    let mut year = 1970;
    while days_to_year(year + 1) <= day && year < 10_000 {
        year += 1;
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
    (year, month)
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

/// Totals for the ISO week containing local day `day`.
pub fn week_totals(store: &Store, day: i64, local_offset_ms: i64) -> Totals {
    let week = iso_week_of(day);
    totals_in_day_range(store, week.start_day, week.start_day + 7, local_offset_ms)
}

/// Totals for the calendar month containing local day `day`.
pub fn month_totals(store: &Store, day: i64, local_offset_ms: i64) -> Totals {
    let (year, month) = month_of(day);
    let first = day_of_month(year, month, 1);
    let next = if month == 12 {
        day_of_month(year + 1, 1, 1)
    } else {
        day_of_month(year, month + 1, 1)
    };
    totals_in_day_range(store, first, next, local_offset_ms)
}

/// Totals for the entries falling in local days `[from, to)`.
pub fn totals_in_day_range(store: &Store, from: i64, to: i64, local_offset_ms: i64) -> Totals {
    let mut t = Totals::default();
    for e in &store.entries {
        let d = local_day_of(e.started_at, local_offset_ms);
        if d >= from && d < to {
            t.total_ms += e.duration_ms();
            t.entry_count += 1;
            *t.per_project.entry(e.project_id.clone()).or_insert(0) += e.duration_ms();
        }
    }
    t
}

/// The local day index of the 1st of `month` in `year`.
fn day_of_month(year: i64, month: u32, day: i64) -> i64 {
    let mut d = days_to_year(year);
    for m in 1..month {
        d += days_in_month(year, m);
    }
    d + day - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entry, EntrySource, MS_PER_DAY, Project};

    const D: i64 = MS_PER_DAY;

    fn store_with(entries: &[(&str, i64, i64)]) -> Store {
        let mut s = Store::default();
        for (i, (pid, a, b)) in entries.iter().enumerate() {
            s.projects.push(Project {
                id: pid.to_string(),
                name: pid.to_string(),
                colour: None,
                archived: false,
            });
            s.entries.push(Entry {
                id: format!("e{i}"),
                project_id: pid.to_string(),
                description: String::new(),
                started_at: *a,
                ended_at: *b,
                source: EntrySource::Manual,
                note: None,
            });
        }
        s
    }

    // --- ISO weeks ---

    #[test]
    fn epoch_was_a_thursday() {
        // 1970-01-01: the anchor every ISO calculation rests on.
        assert_eq!(iso_week_of(0).year, 1970);
        assert_eq!(iso_week_of(0).week, 1);
        assert_eq!(iso_week_of(0).start_day, -3, "Monday 29 Dec 1969");
    }

    #[test]
    fn a_week_starts_on_monday() {
        for day in 0..400 {
            let w = iso_week_of(day);
            let iso_weekday = (day + 3).rem_euclid(7) + 1;
            assert_eq!(
                day - w.start_day,
                iso_weekday - 1,
                "day {day} is not offset from its Monday correctly"
            );
            assert!(w.start_day <= day && day < w.start_day + 7);
        }
    }

    #[test]
    fn days_in_the_same_iso_week_share_a_number() {
        // Week 1 of 1970 runs Mon 29 Dec 1969 (day -3) to Sun 4 Jan (day 3),
        // so days 0..=3 share it and day 4 opens week 2.
        let a = iso_week_of(0);
        for day in 0..=3 {
            assert_eq!(iso_week_of(day), a, "day {day} should share week 1");
        }
        assert_ne!(iso_week_of(4), a, "day 4 is the Monday of week 2");
        assert_eq!(
            iso_week_of(-3),
            a,
            "and day -3 is the Monday that starts it"
        );
        assert_eq!(a.start_day, -3, "week 1 starts on Monday 29 Dec 1969");
    }

    #[test]
    fn week_numbers_stay_within_1_to_53() {
        for day in 0..(365 * 60) {
            let w = iso_week_of(day);
            assert!((1..=53).contains(&w.week), "day {day} gave week {}", w.week);
        }
    }

    #[test]
    fn iso_year_rolls_at_the_thursday_not_january_first() {
        // 2021-01-01 was a Friday, so it belongs to the week whose Thursday
        // is 2020-12-31 -- i.e. the previous ISO year.
        let jan1_2021 = day_of_month(2021, 1, 1);
        let w = iso_week_of(jan1_2021);
        assert_eq!(w.year, 2020, "ISO year must roll back to 2020");
        assert_eq!(w.week, 53, "2020 had 53 ISO weeks");
    }

    #[test]
    fn iso_year_is_stable_across_a_normal_week() {
        // Mid-year weeks report the calendar year plainly.
        let july = day_of_month(2021, 7, 15);
        let w = iso_week_of(july);
        assert_eq!(w.year, 2021);
        assert!((26..=30).contains(&w.week), "got week {}", w.week);
    }

    // --- months ---

    #[test]
    fn month_of_finds_january_1970() {
        assert_eq!(month_of(0), (1970, 1));
    }

    #[test]
    fn february_has_29_days_in_a_leap_year() {
        // 2024 is a leap year, so 29 Feb exists and 1 Mar is the 61st day.
        assert_eq!(month_of(day_of_month(2024, 2, 29)), (2024, 2));
        assert_eq!(month_of(day_of_month(2024, 3, 1)), (2024, 3));
    }

    #[test]
    fn february_has_28_days_in_a_non_leap_year() {
        // 2023 is not, so 28 Feb is the last day of the month.
        assert_eq!(month_of(day_of_month(2023, 2, 28)), (2023, 2));
        assert_eq!(month_of(day_of_month(2023, 3, 1)), (2023, 3));
    }

    #[test]
    fn month_lengths_are_right() {
        for (y, m, len) in [
            (2023, 1, 31),
            (2023, 2, 28),
            (2024, 2, 29),
            (2023, 4, 30),
            (2023, 12, 31),
            (2100, 2, 28),
            (2000, 2, 29),
        ] {
            assert_eq!(
                month_of(day_of_month(y, m, len)),
                (y, m),
                "{y}-{m} day {len}"
            );
            assert_eq!(
                month_of(day_of_month(y, m, len + 1)),
                if m == 12 { (y + 1, 1) } else { (y, m + 1) },
                "day {len} + 1 must leave the month"
            );
        }
    }

    // --- totals ---

    #[test]
    fn week_totals_sum_durations() {
        let s = store_with(&[("p1", 0, 3_600_000), ("p1", 7_200_000, 9_000_000)]);
        let t = week_totals(&s, 0, 0);
        assert_eq!(t.total_ms, 3_600_000 + 1_800_000);
        assert_eq!(t.entry_count, 2);
    }

    #[test]
    fn overlapping_entries_both_count() {
        let s = store_with(&[
            ("p1", 0, 8 * 3_600_000),
            ("p1", 4 * 3_600_000, 12 * 3_600_000),
        ]);
        let t = week_totals(&s, 0, 0);
        assert_eq!(t.total_ms, 8 * 3_600_000 + 8 * 3_600_000);
    }

    #[test]
    fn per_project_totals_are_separate() {
        let s = store_with(&[("p1", 0, 3_600_000), ("p2", 3_600_000, 5_400_000)]);
        let t = week_totals(&s, 0, 0);
        assert_eq!(t.total_ms, 3_600_000 + 1_800_000);
        assert_eq!(t.per_project.get("p1"), Some(&3_600_000));
        assert_eq!(t.per_project.get("p2"), Some(&1_800_000));
    }

    #[test]
    fn ranked_orders_by_time_longest_first() {
        let s = store_with(&[
            ("small", 0, 60_000),
            ("big", 60_000, 7_260_000),
            ("mid", 0, 600_000),
        ]);
        let t = week_totals(&s, 0, 0);
        let order: Vec<&str> = t.ranked().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(order, ["big", "mid", "small"]);
    }

    #[test]
    fn entries_outside_the_week_are_excluded() {
        // An entry a fortnight later must not appear in this week's total.
        let s = store_with(&[("p1", 0, 3_600_000), ("p1", 14 * D, 15 * D)]);
        let t = week_totals(&s, 0, 0);
        assert_eq!(t.entry_count, 1);
        assert_eq!(t.total_ms, 3_600_000);
    }

    #[test]
    fn a_week_spanning_a_month_boundary_is_split_correctly() {
        // An entry on the last day of a month belongs to that month; the same
        // ISO week can straddle two months, and the month view must not
        // double-count.
        let s = store_with(&[("p1", 0, 3_600_000)]);
        let t = month_totals(&s, 0, 0); // Jan 1970
        assert_eq!(t.total_ms, 3_600_000);
    }

    #[test]
    fn local_offset_can_move_an_entry_into_the_next_week() {
        // 23:30 UTC is the next day in UTC+1, which can cross a week boundary
        // -- the DST-adjacent class of bug PLAN.md warns about. Week 1 of 1970
        // starts on Monday 29 Dec 1969 and ends Sunday 4 Jan, so an entry that
        // rolls from local day 3 (Sun) to local day 4 (Mon) leaves the week.
        let late = 3 * D + 23 * 3_600_000 + 1_800_000; // Sunday 23:30 local
        let s = store_with(&[("p1", late, late + 60_000)]);
        assert_eq!(
            week_totals(&s, 3, 0).entry_count,
            1,
            "in UTC the entry is on the Sunday of week 1"
        );
        // At UTC+1 the entry sits on local day 4, the Monday that opens week 2.
        assert_eq!(
            week_totals(&s, 4, 3_600_000).entry_count,
            1,
            "so it now belongs to week 2"
        );
        assert_eq!(
            week_totals(&s, 3, 3_600_000).entry_count,
            0,
            "and no longer to week 1, whose last day is day 3"
        );
    }

    #[test]
    fn empty_store_totals_are_zero() {
        let s = Store::default();
        assert_eq!(week_totals(&s, 0, 0).total_ms, 0);
        assert!(week_totals(&s, 0, 0).per_project.is_empty());
    }
}
