/* core/parse.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Parsing the times and durations a person types.
//!
//! Both front ends accept the same spellings (REQUIREMENTS §5: the manual
//! entry dialog and the CLI are one form with different defaults), so the
//! parsing lives here rather than once per client. A spelling parsed twice is
//! a disagreement waiting to happen — and the CLI and GUI would be free to
//! disagree, which is exactly what the shared core exists to prevent.
//!
//! Like the rest of the core this takes the clock as a parameter (`now_ms`)
//! and reads nothing itself. The service owns the timezone and reports its
//! offset; callers pass it in (the CLI and GUI read it from the snapshot).

use crate::model::MS_PER_DAY;
use crate::tz::Tz;

/// Why a typed moment or duration was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("{0} cannot be empty")]
    EmptyMoment(String),
    #[error("{what}: '{text}' is not a number of minutes ago")]
    BadRelative { what: String, text: String },
    #[error("{what}: '{text}' is neither -90, 90, now nor HH:MM")]
    BadShape { what: String, text: String },
    #[error("{what}: '{text}' has a bad hour")]
    BadHour { what: String, text: String },
    #[error("{what}: '{text}' has a bad minute")]
    BadMinute { what: String, text: String },
    #[error("{what}: '{text}' is not a real time of day")]
    BadTimeOfDay { what: String, text: String },
    #[error("{what}: '{text}' is not a calendar day (use YYYY-MM-DD)")]
    BadDay { what: String, text: String },
    #[error("duration cannot be empty")]
    EmptyDuration,
    #[error("'{0}': expected a number before '{1}'")]
    ExpectedNumber(String, char),
    #[error("'{0}': unknown unit '{1}' (use h, m or s)")]
    UnknownUnit(String, char),
    #[error("'{0}': trailing number is not a number")]
    BadTrailingNumber(String),
    #[error("'{0}' has no unit (try 30m, 1h30m or 45s)")]
    NoUnit(String),
    #[error("'{0}' must be a positive duration")]
    NonPositive(String),
    /// A typed value whose millisecond result does not fit an `i64` (#98).
    /// Its own variant so debug and release agree: unchecked, debug builds
    /// panicked here while release builds wrapped to the wrong end of time.
    #[error("{what}: '{text}' is too large")]
    Overflow { what: String, text: String },
    /// A colour that is not six hex digits. The wording is the CLI's
    /// established one; both front ends now share this parse (#104).
    #[error("'{0}' is not RRGGBB hex (e.g. 2ea043)")]
    BadColour(String),
}

pub type ParseResult<T> = Result<T, ParseError>;

/// Parse a moment given by a person, in milliseconds since the epoch.
///
/// Three forms, covering what a person actually means:
///
/// - `-90` — minutes relative to now, so `-90` is 90 minutes ago;
/// - `90` — a bare positive number reads as minutes from now;
/// - `09:30` — today at that wall-clock time, in local time;
/// - `now` — this instant.
///
/// `now_ms` is the caller's clock reading and `tz` the service's zone:
/// `HH:MM` anchors on local midnight, not UTC midnight, or entries land hours
/// out for anyone east or west of Greenwich. Midnight itself comes from the
/// zone (`Tz::day_start`), so days either side of a DST transition still
/// anchor on their own midnight.
///
/// Calendar days (`YYYY-MM-DD`) are deliberately refused here: only the
/// export anchor path parses them (see [`parse_anchor_day`]), so `add -S`,
/// edit, split and friends keep their main-branch behaviour.
///
/// A leading `-` is why the CLI passes these through clap with
/// `allow_hyphen_values`: `-90` is a perfectly ordinary thing to type.
pub fn parse_moment(text: &str, what: &str, now_ms: i64, tz: &Tz) -> ParseResult<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ParseError::EmptyMoment(what.to_string()));
    }
    if text.eq_ignore_ascii_case("now") {
        return Ok(now_ms);
    }

    // Relative minutes, checked end to end (#98): a value that overflows
    // `i64` milliseconds — or lands the result outside it — is an error,
    // identically in debug and release.
    let overflow = || ParseError::Overflow {
        what: what.to_string(),
        text: text.to_string(),
    };
    if let Some(rest) = text.strip_prefix('-') {
        let mins: i64 = rest.parse().map_err(|e| {
            if is_int_overflow(&e) {
                overflow()
            } else {
                ParseError::BadRelative {
                    what: what.to_string(),
                    text: text.to_string(),
                }
            }
        })?;
        return mins
            .checked_mul(60_000)
            .and_then(|ms| now_ms.checked_sub(ms))
            .ok_or_else(overflow);
    }
    if let Ok(mins) = text.parse::<i64>() {
        // A bare positive number reads as "minutes from now".
        return mins
            .checked_mul(60_000)
            .and_then(|ms| now_ms.checked_add(ms))
            .ok_or_else(overflow);
    }
    // A bare number too big for `i64` is "too large", not a bad shape.
    if let Err(e) = text.parse::<i64>() {
        if is_int_overflow(&e) {
            return Err(overflow());
        }
    }

    let (h, m) = text.split_once(':').ok_or_else(|| ParseError::BadShape {
        what: what.to_string(),
        text: text.to_string(),
    })?;
    let h: i64 = h.parse().map_err(|_| ParseError::BadHour {
        what: what.to_string(),
        text: text.to_string(),
    })?;
    let m: i64 = m.parse().map_err(|_| ParseError::BadMinute {
        what: what.to_string(),
        text: text.to_string(),
    })?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return Err(ParseError::BadTimeOfDay {
            what: what.to_string(),
            text: text.to_string(),
        });
    }

    let local_midnight = tz.day_start(tz.day_of(now_ms));
    // `h`/`m` are bounded above, so only the final add can overflow, and
    // only for a caller-supplied `now_ms` near the `i64` edges (#98).
    local_midnight
        .checked_add(h * 3_600_000 + m * 60_000)
        .ok_or_else(overflow)
}

/// Whether an `i64` parse failed because the value does not fit, as opposed
/// to not being a number at all.
fn is_int_overflow(e: &std::num::ParseIntError) -> bool {
    use std::num::IntErrorKind;
    matches!(
        e.kind(),
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow
    )
}

/// Parse an export anchor day (`YYYY-MM-DD`) to local midday that day.
///
/// The day resolves to 12:00 local wall-clock time in `tz`, not midnight:
/// midnight sits on a day edge (and on DST transition days the offset at
/// midnight differs from the offset at noon), so anchoring there risks
/// bucketing into the wrong day. Midday is safely inside the intended day
/// in every zone.
///
/// Only the export anchor path reaches this (`--at` on the CLI, the typed
/// day on the Export tab): [`parse_moment`] deliberately refuses days so
/// entry moments keep their main-branch behaviour.
pub fn parse_anchor_day(text: &str, what: &str, tz: &Tz) -> ParseResult<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ParseError::EmptyMoment(what.to_string()));
    }
    let bad = || ParseError::BadDay {
        what: what.to_string(),
        text: text.to_string(),
    };
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    if parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(bad());
    }
    if !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return Err(bad());
    }
    let y: i32 = parts[0].parse().map_err(|_| bad())?;
    let m: u32 = parts[1].parse().map_err(|_| bad())?;
    let d: u32 = parts[2].parse().map_err(|_| bad())?;
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d).ok_or_else(bad)?;
    // The supported range starts at year 0001 (#97): `0000-…` parses as a
    // real chrono date but names an instant the rest of the app refuses.
    if y < 1 {
        return Err(bad());
    }
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch exists");
    let day = date.signed_duration_since(epoch).num_days();
    // Local noon that day: find the UTC instant whose wall clock is 12:00.
    // Iterates to a fixed point like `Tz::day_start`, so transition days
    // (where midnight and noon offsets differ) still land on 12:00 local.
    let noon_local = day * MS_PER_DAY + 12 * 3_600_000;
    let mut utc = noon_local - tz.offset_at_ms(noon_local);
    for _ in 0..4 {
        let next = noon_local - tz.offset_at_ms(utc);
        if next == utc {
            break;
        }
        utc = next;
    }
    Ok(utc)
}

/// Parse a duration like `90m`, `1h30m` or `45s`, in milliseconds.
///
/// A trailing number with no unit reads as minutes: a bare `30` almost
/// certainly means 30 minutes.
pub fn parse_duration(text: &str) -> ParseResult<i64> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err(ParseError::EmptyDuration);
    }

    let mut total: i64 = 0;
    let mut number = String::new();
    let mut saw_unit = false;
    // Checked throughout (#98): each term and the running total must fit
    // `i64` milliseconds, in debug and release alike.
    let overflow = || ParseError::Overflow {
        what: "duration".to_string(),
        text: text.clone(),
    };

    for ch in text.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let n: i64 = number.parse().map_err(|e| {
            if is_int_overflow(&e) {
                overflow()
            } else {
                ParseError::ExpectedNumber(text.clone(), ch)
            }
        })?;
        number.clear();
        let factor = match ch {
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1_000,
            _ => return Err(ParseError::UnknownUnit(text.clone(), ch)),
        };
        total = n
            .checked_mul(factor)
            .and_then(|term| total.checked_add(term))
            .ok_or_else(overflow)?;
        saw_unit = true;
    }

    if !number.is_empty() {
        let n: i64 = number.parse().map_err(|e| {
            if is_int_overflow(&e) {
                overflow()
            } else {
                ParseError::BadTrailingNumber(text.clone())
            }
        })?;
        total = n
            .checked_mul(60_000)
            .and_then(|term| total.checked_add(term))
            .ok_or_else(overflow)?;
        saw_unit = true;
    }

    if !saw_unit {
        return Err(ParseError::NoUnit(text.clone()));
    }
    if total <= 0 {
        return Err(ParseError::NonPositive(text.clone()));
    }
    Ok(total)
}

/// Parse an `RRGGBB` colour like `2ea043` (leading `#` allowed), as stored
/// on a project (`0xRRGGBB`).
///
/// One spelling for both front ends (#104): the CLI's `recolour` and the
/// GUI's recolour prompt accept exactly the same text, trimmed, `#`
/// optional, six hex digits.
pub fn parse_colour(text: &str) -> ParseResult<u32> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ParseError::BadColour(text.to_string()));
    }
    Ok(u32::from_str_radix(hex, 16).expect("six hex digits always fit"))
}

/// This entry's local wall-clock time as `HH:MM`, for prefilling an edit form.
///
/// The value parses back through [`parse_moment`] on the same local day, so a
/// dialog can show "keep 09:30" and mean it — provided the entry is from
/// today. Callers must only use this for entries whose local day is today;
/// otherwise `HH:MM` would silently move the endpoint to today.
pub fn format_local_hm(utc_ms: i64, tz: &Tz) -> String {
    let rem = utc_ms
        .saturating_add(tz.offset_at_ms(utc_ms))
        .rem_euclid(MS_PER_DAY);
    format!("{:02}:{:02}", rem / 3_600_000, (rem % 3_600_000) / 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> Tz {
        Tz::utc()
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration("45s").unwrap(), 45_000);
        assert_eq!(parse_duration("1h30m").unwrap(), 5_400_000);
    }

    #[test]
    fn durations_are_case_insensitive() {
        assert_eq!(parse_duration("30M").unwrap(), 1_800_000);
    }

    #[test]
    fn a_bare_number_reads_as_minutes() {
        assert_eq!(parse_duration("30").unwrap(), 1_800_000);
    }

    #[test]
    fn zero_and_non_durations_are_refused() {
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn an_unknown_unit_is_named_in_the_error() {
        let e = parse_duration("30d").unwrap_err().to_string();
        assert!(e.contains('d'), "should name the bad unit: {e}");
    }

    #[test]
    fn now_means_now() {
        assert_eq!(parse_moment("now", "end", 1_000, &utc()).unwrap(), 1_000);
        assert_eq!(parse_moment("NOW", "end", 1_000, &utc()).unwrap(), 1_000);
        assert_eq!(
            parse_moment("  now  ", "end", 1_000, &utc()).unwrap(),
            1_000
        );
    }

    #[test]
    fn minutes_ago_parses_relative_to_now() {
        let t = parse_moment("-90", "start", 10_000_000, &utc()).unwrap();
        assert_eq!(t, 10_000_000 - 5_400_000);
    }

    #[test]
    fn minutes_from_now_parses_forward() {
        assert_eq!(
            parse_moment("30", "start", 10_000_000, &utc()).unwrap(),
            11_800_000
        );
    }

    #[test]
    fn a_wall_clock_time_lands_today() {
        // now = 12:00 UTC on day 1; 09:30 must fall inside that UTC day.
        let noon_day1 = MS_PER_DAY + 12 * 3_600_000;
        let t = parse_moment("09:30", "start", noon_day1, &utc()).unwrap();
        assert_eq!(t, MS_PER_DAY + 9 * 3_600_000 + 30 * 60_000);
    }

    #[test]
    fn a_wall_clock_time_is_local_not_utc() {
        // UTC+2: anchoring on UTC midnight would put 00:30 two hours out.
        let tz = Tz::fixed_ms(2 * 3_600_000);
        let now = MS_PER_DAY + 12 * 3_600_000; // 12:00 UTC, 14:00 local
        let t = parse_moment("00:30", "start", now, &tz).unwrap();
        assert_eq!(tz.day_of(t), tz.day_of(now));
        assert_eq!(format_local_hm(t, &tz), "00:30");
    }

    #[test]
    fn a_wall_clock_time_uses_the_named_zone() {
        // Rome in summer is +2: 00:30 local is 22:30 UTC the day before.
        let rome = Tz::parse("Europe/Rome").unwrap();
        // 2026-07-06 12:00 Rome = 10:00 UTC.
        let now = 1_783_332_000_000;
        let t = parse_moment("00:30", "start", now, &rome).unwrap();
        assert_eq!(rome.day_of(t), rome.day_of(now));
        assert_eq!(format_local_hm(t, &rome), "00:30");
    }

    #[test]
    fn impossible_times_are_refused() {
        assert!(parse_moment("25:00", "start", 0, &utc()).is_err());
        assert!(parse_moment("09:70", "start", 0, &utc()).is_err());
        assert!(parse_moment("nonsense", "start", 0, &utc()).is_err());
        assert!(parse_moment("", "start", 0, &utc()).is_err());
        assert!(parse_moment("-soon", "start", 0, &utc()).is_err());
    }

    #[test]
    fn the_error_names_the_field() {
        assert!(
            parse_moment("99:99", "end", 0, &utc())
                .unwrap_err()
                .to_string()
                .contains("end")
        );
    }

    #[test]
    fn a_calendar_day_resolves_to_local_midday_utc() {
        // 2026-10-12 is a Monday; midday UTC is 12:00 that day.
        let t = parse_anchor_day("2026-10-12", "anchor", &utc()).unwrap();
        assert_eq!(t, 1_791_806_400_000);
        assert_eq!(format_local_hm(t, &utc()), "12:00");
        assert_eq!(utc().day_of(t), 20_738);
    }

    #[test]
    fn a_calendar_day_resolves_to_midday_in_a_named_zone_summer_and_winter() {
        let rome = Tz::parse("Europe/Rome").unwrap();
        // 2026-07-15 12:00 Rome (CEST, +2) = 10:00 UTC.
        let summer = parse_anchor_day("2026-07-15", "anchor", &rome).unwrap();
        assert_eq!(summer, 1_784_109_600_000);
        assert_eq!(format_local_hm(summer, &rome), "12:00");
        // 2026-01-15 12:00 Rome (CET, +1) = 11:00 UTC.
        let winter = parse_anchor_day("2026-01-15", "anchor", &rome).unwrap();
        assert_eq!(winter, 1_768_474_800_000);
        assert_eq!(format_local_hm(winter, &rome), "12:00");
    }

    #[test]
    fn a_calendar_day_survives_dst_transition_days() {
        let rome = Tz::parse("Europe/Rome").unwrap();
        // Spring forward 2026-03-29 (23h day): noon is CEST (+2).
        let spring = parse_anchor_day("2026-03-29", "anchor", &rome).unwrap();
        assert_eq!(spring, 1_774_778_400_000);
        assert_eq!(format_local_hm(spring, &rome), "12:00");
        // Fall back 2026-10-25 (25h day): noon is CET (+1).
        let autumn = parse_anchor_day("2026-10-25", "anchor", &rome).unwrap();
        assert_eq!(autumn, 1_792_926_000_000);
        assert_eq!(format_local_hm(autumn, &rome), "12:00");
    }

    #[test]
    fn bad_anchor_days_name_the_value() {
        for bad in [
            "2026-13-01",
            "2026-02-30",
            "2026-10",
            "2026/10/12",
            "2026-1-5",
            "not-a-day",
        ] {
            let e = parse_anchor_day(bad, "anchor", &utc())
                .unwrap_err()
                .to_string();
            assert!(e.contains("anchor"), "{bad:?} should name the field: {e}");
            assert!(e.contains(bad), "{bad:?} should name the value: {e}");
        }
        // Empty names the field.
        let e = parse_anchor_day("", "anchor", &utc())
            .unwrap_err()
            .to_string();
        assert!(e.contains("anchor"), "empty should name the field: {e}");
        let e = parse_anchor_day("   ", "anchor", &utc())
            .unwrap_err()
            .to_string();
        assert!(e.contains("anchor"), "blank should name the field: {e}");
    }

    #[test]
    fn monday_0030_stays_in_the_intended_week() {
        use crate::aggregate::iso_week_of;
        let rome = Tz::parse("Europe/Rome").unwrap();
        // 2026-10-12 is a Monday. The anchor resolves to midday, well inside
        // the day, while a 00:30 entry sits near the week edge.
        let anchor = parse_anchor_day("2026-10-12", "anchor", &rome).unwrap();
        let monday_0030 = 1_791_757_800_000; // 2026-10-12 00:30 Rome (+2)
        assert_eq!(format_local_hm(monday_0030, &rome), "00:30");
        assert_eq!(
            rome.day_of(anchor),
            rome.day_of(monday_0030),
            "midday anchor and 00:30 must share the local day"
        );
        assert_eq!(
            iso_week_of(rome.day_of(anchor)),
            iso_week_of(rome.day_of(monday_0030)),
            "and therefore the same ISO week"
        );
    }

    #[test]
    fn calendar_days_are_refused_as_moments() {
        // Regression: `YYYY-MM-DD` leaked into every moment field via
        // `parse_moment`, so `add -S`, edit and split accepted midday days.
        // Days belong to the export-anchor path only (`parse_anchor_day`).
        for day in ["2026-10-12", "2026-01-15", "2026-13-01", "2026/10/12"] {
            for what in ["start", "end", "at"] {
                let e = parse_moment(day, what, 0, &utc()).unwrap_err().to_string();
                assert!(
                    e.contains(what),
                    "{day:?} as {what} should name the field: {e}"
                );
                assert!(e.contains(day), "{day:?} should name the value: {e}");
            }
        }
    }

    #[test]
    fn hm_formats_midnight_and_round_trips() {
        assert_eq!(format_local_hm(0, &utc()), "00:00");
        assert_eq!(format_local_hm(0, &Tz::fixed_ms(3_600_000)), "01:00");
        // 09:30 UTC parses back to itself on the same day.
        let t = 9 * 3_600_000 + 30 * 60_000;
        assert_eq!(format_local_hm(t, &utc()), "09:30");
        assert_eq!(
            parse_moment(&format_local_hm(t, &utc()), "start", t, &utc()).unwrap(),
            t
        );
    }

    #[test]
    fn overflowing_typed_values_error_instead_of_wrapping() {
        // #98: debug builds panicked and release builds wrapped on these.
        assert!(parse_moment("-99999999999999999", "start", 0, &utc()).is_err());
        assert!(parse_moment("99999999999999999", "start", 0, &utc()).is_err());
        assert!(parse_duration("9999999999999999999h").is_err());
        assert!(parse_duration("3000000000000h3000000000000h").is_err());
        // The refusal is the typed overflow error, naming field and value.
        assert_eq!(
            parse_duration("9999999999999999999h").unwrap_err(),
            ParseError::Overflow {
                what: "duration".into(),
                text: "9999999999999999999h".into()
            }
        );
        assert_eq!(
            parse_moment("99999999999999999", "start", 0, &utc()).unwrap_err(),
            ParseError::Overflow {
                what: "start".into(),
                text: "99999999999999999".into()
            }
        );
        // Near-boundary values that do fit still parse.
        assert!(parse_duration("1000000h").is_ok());
        assert!(parse_moment("-1000000", "start", 0, &utc()).is_ok());
    }

    #[test]
    fn colours_parse_as_rrggbb_hex() {
        assert_eq!(parse_colour("2ea043").unwrap(), 0x2ea043);
        assert_eq!(parse_colour("#2EA043").unwrap(), 0x2ea043);
        assert_eq!(parse_colour("  ff0000  ").unwrap(), 0xff0000);
        for bad in ["", "red", "12345", "1234567", "zzzzzz", "#12 34"] {
            assert_eq!(
                parse_colour(bad).unwrap_err(),
                ParseError::BadColour(bad.to_string()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn anchor_days_stay_inside_the_supported_range() {
        // #97: year 0000 is a real chrono date but outside years 0001-9999.
        assert!(parse_anchor_day("0000-01-01", "anchor", &utc()).is_err());
        assert!(parse_anchor_day("0000-12-31", "anchor", &utc()).is_err());
        // Both range edges resolve to local midday on the named day.
        let first = parse_anchor_day("0001-01-01", "anchor", &utc()).unwrap();
        assert_eq!(first, crate::model::MIN_INSTANT_MS + 12 * 3_600_000);
        let last = parse_anchor_day("9999-12-31", "anchor", &utc()).unwrap();
        assert_eq!(utc().day_of(last), crate::model::MAX_DAY);
        assert_eq!(format_local_hm(last, &utc()), "12:00");
    }
}
