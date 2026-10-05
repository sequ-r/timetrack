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

use crate::model::Store;
use crate::tz::Tz;
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
    // ... and a matching downward walk: without it every pre-1970 Thursday
    // stayed in "1970", and the week number below could go negative and
    // wrap around the `as u64` cast.
    while days_to_year(year) > thursday && year > -10_000 {
        year -= 1;
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
/// are counted exactly, in both directions: the `year..1970` range for
/// pre-epoch years would otherwise be empty and every date before 1970 would
/// land in January 1970.
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

/// The calendar month containing local day `day`, as `(year, month)`.
pub fn month_of(day: i64) -> (i64, u32) {
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
pub fn week_totals(store: &Store, day: i64, tz: &Tz) -> Totals {
    let week = iso_week_of(day);
    totals_in_day_range(store, week.start_day, week.start_day + 7, tz)
}

/// Totals for the calendar month containing local day `day`.
pub fn month_totals(store: &Store, day: i64, tz: &Tz) -> Totals {
    let (year, month) = month_of(day);
    let first = day_of_month(year, month, 1);
    let next = if month == 12 {
        day_of_month(year + 1, 1, 1)
    } else {
        day_of_month(year, month + 1, 1)
    };
    totals_in_day_range(store, first, next, tz)
}

/// Totals for the entries falling in local days `[from, to)`.
///
/// Each entry buckets by the offset in force at its own start, so a week
/// spanning a DST transition still splits exactly on local midnights.
pub fn totals_in_day_range(store: &Store, from: i64, to: i64, tz: &Tz) -> Totals {
    let mut t = Totals::default();
    for e in &store.entries {
        let d = tz.day_of(e.started_at);
        if d >= from && d < to {
            t.total_ms += e.duration_ms();
            t.entry_count += 1;
            *t.per_project.entry(e.project_id.clone()).or_insert(0) += e.duration_ms();
        }
    }
    t
}

/// Totals across all time, overall and per project.
///
/// The Projects tab's all-time column comes from the service rather than
/// being re-summed by each client, so both UIs agree even when the snapshot
/// carries only recent entries. Like every other total this is a sum of
/// durations (§4): overlapping entries count twice.
pub fn all_totals(store: &Store) -> Totals {
    let mut t = Totals::default();
    for e in &store.entries {
        t.total_ms += e.duration_ms();
        t.entry_count += 1;
        *t.per_project.entry(e.project_id.clone()).or_insert(0) += e.duration_ms();
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
    use crate::tz::Tz;

    const D: i64 = MS_PER_DAY;

    fn utc() -> Tz {
        Tz::utc()
    }

    fn fixed(offset_ms: i64) -> Tz {
        Tz::fixed_ms(offset_ms)
    }

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
    fn month_of_handles_pre_epoch_days() {
        // Day -1 is 31 Dec 1969, not January 1970: the year walk has to run
        // backwards too, and `days_to_year` has to count leap days there.
        assert_eq!(month_of(-1), (1969, 12));
        assert_eq!(month_of(day_of_month(1969, 12, 31)), (1969, 12));
        assert_eq!(month_of(day_of_month(1969, 1, 1)), (1969, 1));
        assert_eq!(month_of(day_of_month(1968, 2, 29)), (1968, 2));
        assert_eq!(month_of(day_of_month(1970, 1, 1) - 1), (1969, 12));
    }

    #[test]
    fn iso_weeks_are_sane_before_the_epoch() {
        // Same downward-walk bug as `month_of`: without it the year stayed
        // 1970 and far-enough-back Thursdays wrapped the week number.
        for day in -800..0 {
            let w = iso_week_of(day);
            assert!((1..=53).contains(&w.week), "day {day} gave week {}", w.week);
            assert!(
                w.start_day <= day && day < w.start_day + 7,
                "day {day} is outside its own week"
            );
        }
        // 1969-12-29 was a Monday opening ISO week 1 of 1970.
        let w = iso_week_of(day_of_month(1969, 12, 29));
        assert_eq!((w.year, w.week), (1970, 1));
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
        let t = week_totals(&s, 0, &utc());
        assert_eq!(t.total_ms, 3_600_000 + 1_800_000);
        assert_eq!(t.entry_count, 2);
    }

    #[test]
    fn overlapping_entries_both_count() {
        let s = store_with(&[
            ("p1", 0, 8 * 3_600_000),
            ("p1", 4 * 3_600_000, 12 * 3_600_000),
        ]);
        let t = week_totals(&s, 0, &utc());
        assert_eq!(t.total_ms, 8 * 3_600_000 + 8 * 3_600_000);
    }

    #[test]
    fn per_project_totals_are_separate() {
        let s = store_with(&[("p1", 0, 3_600_000), ("p2", 3_600_000, 5_400_000)]);
        let t = week_totals(&s, 0, &utc());
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
        let t = week_totals(&s, 0, &utc());
        let order: Vec<&str> = t.ranked().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(order, ["big", "mid", "small"]);
    }

    #[test]
    fn entries_outside_the_week_are_excluded() {
        // An entry a fortnight later must not appear in this week's total.
        let s = store_with(&[("p1", 0, 3_600_000), ("p1", 14 * D, 15 * D)]);
        let t = week_totals(&s, 0, &utc());
        assert_eq!(t.entry_count, 1);
        assert_eq!(t.total_ms, 3_600_000);
    }

    #[test]
    fn a_week_spanning_a_month_boundary_is_split_correctly() {
        // An entry on the last day of a month belongs to that month; the same
        // ISO week can straddle two months, and the month view must not
        // double-count.
        let s = store_with(&[("p1", 0, 3_600_000)]);
        let t = month_totals(&s, 0, &utc()); // Jan 1970
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
            week_totals(&s, 3, &utc()).entry_count,
            1,
            "in UTC the entry is on the Sunday of week 1"
        );
        // At UTC+1 the entry sits on local day 4, the Monday that opens week 2.
        let plus1 = fixed(3_600_000);
        assert_eq!(
            week_totals(&s, 4, &plus1).entry_count,
            1,
            "so it now belongs to week 2"
        );
        assert_eq!(
            week_totals(&s, 3, &plus1).entry_count,
            0,
            "and no longer to week 1, whose last day is day 3"
        );
    }

    #[test]
    fn named_zone_buckets_by_its_own_offset_per_instant() {
        // 2026-07-05 22:30 UTC is Sunday in UTC but Monday 00:30 in Rome
        // (CEST, +2): the two bucket into different ISO weeks. A fixed-offset
        // fallback of UTC gets this wrong; the tz database gets it right.
        let rome = Tz::parse("Europe/Rome").expect("Europe/Rome must parse");
        let utc_ms = 1_783_290_600_000; // 2026-07-05 22:30 UTC
        assert_eq!(rome.offset_at_ms(utc_ms), 7_200_000);
        let rome_day = rome.day_of(utc_ms);
        let utc_day = utc().day_of(utc_ms);
        assert_eq!(rome_day, utc_day + 1, "00:30 next day in Rome");
        assert_ne!(
            iso_week_of(utc_day),
            iso_week_of(rome_day),
            "Sunday week 27 vs Monday week 28"
        );
        let s = store_with(&[("p1", utc_ms, utc_ms + 60_000)]);
        assert_eq!(week_totals(&s, utc_day, &utc()).entry_count, 1);
        assert_eq!(
            week_totals(&s, utc_day, &rome).entry_count,
            0,
            "in Rome the entry is not on the Sunday"
        );
        assert_eq!(week_totals(&s, rome_day, &rome).entry_count, 1);
    }

    #[test]
    fn named_zone_tracks_dst_across_seasons() {
        // The same zone is +1 in winter and +2 in summer, and one `Tz` must
        // bucket both correctly — a single fixed offset cannot.
        let rome = Tz::parse("Europe/Rome").unwrap();
        let winter = 1_768_478_400_000; // 2026-01-15 12:00 UTC
        let summer = 1_784_116_800_000; // 2026-07-15 12:00 UTC
        assert_eq!(rome.offset_at_ms(winter), 3_600_000);
        assert_eq!(rome.offset_at_ms(summer), 7_200_000);
        // Late-evening UTC instants land on the next local day in both
        // seasons, but by different offsets.
        let winter_late = 1_767_569_400_000; // 2026-01-04 23:30 UTC
        let summer_late = 1_783_290_600_000; // 2026-07-05 22:30 UTC
        assert_eq!(rome.day_of(winter_late), utc().day_of(winter_late) + 1);
        assert_eq!(rome.day_of(summer_late), utc().day_of(summer_late) + 1);
    }

    #[test]
    fn named_zone_handles_the_transition_days() {
        // Spring forward 2026-03-29 01:00 UTC (CET -> CEST) and fall back
        // 2026-10-25 01:00 UTC (CEST -> CET): the offset changes mid-day, and
        // per-instant lookup follows it.
        let rome = Tz::parse("Europe/Rome").unwrap();
        assert_eq!(rome.offset_at_ms(1_774_744_200_000), 3_600_000); // Mar 29 00:30 UTC
        assert_eq!(rome.offset_at_ms(1_774_747_800_000), 7_200_000); // Mar 29 01:30 UTC
        assert_eq!(rome.offset_at_ms(1_792_888_200_000), 7_200_000); // Oct 25 00:30 UTC
        assert_eq!(rome.offset_at_ms(1_792_895_400_000), 3_600_000); // Oct 25 02:30 UTC
        // Both sides of each transition still bucket as the same local day:
        // a 23h/25h day is one day, not two.
        let spring_day = rome.day_of(1_774_744_200_000);
        assert_eq!(rome.day_of(1_774_747_800_000), spring_day);
        let autumn_day = rome.day_of(1_792_888_200_000);
        assert_eq!(rome.day_of(1_792_895_400_000), autumn_day);
    }

    #[test]
    fn empty_store_totals_are_zero() {
        let s = Store::default();
        assert_eq!(week_totals(&s, 0, &utc()).total_ms, 0);
        assert!(week_totals(&s, 0, &utc()).per_project.is_empty());
    }

    #[test]
    fn all_time_totals_span_every_entry() {
        // Entries weeks apart still land in one total, split per project.
        // Overlapping entries count twice here too (§4).
        let s = store_with(&[
            ("p1", 0, 3_600_000),
            ("p2", 3_600_000, 5_400_000),
            ("p1", 30 * D, 30 * D + 3_600_000),
        ]);
        let t = all_totals(&s);
        assert_eq!(t.total_ms, 3_600_000 + 1_800_000 + 3_600_000);
        assert_eq!(t.entry_count, 3);
        assert_eq!(t.per_project.get("p1"), Some(&7_200_000));
        assert_eq!(t.per_project.get("p2"), Some(&1_800_000));
    }
}
