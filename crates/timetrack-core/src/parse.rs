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
/// `now_ms` is the caller's clock reading and `local_offset_ms` the service's
/// UTC offset: `HH:MM` anchors on local midnight, not UTC midnight, or
/// entries land hours out for anyone east or west of Greenwich.
///
/// A leading `-` is why the CLI passes these through clap with
/// `allow_hyphen_values`: `-90` is a perfectly ordinary thing to type.
pub fn parse_moment(text: &str, what: &str, now_ms: i64, local_offset_ms: i64) -> ParseResult<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ParseError::EmptyMoment(what.to_string()));
    }
    if text.eq_ignore_ascii_case("now") {
        return Ok(now_ms);
    }

    if let Some(rest) = text.strip_prefix('-') {
        let mins: i64 = rest.parse().map_err(|_| ParseError::BadRelative {
            what: what.to_string(),
            text: text.to_string(),
        })?;
        return Ok(now_ms - mins * 60_000);
    }
    if let Ok(mins) = text.parse::<i64>() {
        // A bare positive number reads as "minutes from now".
        return Ok(now_ms + mins * 60_000);
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

    let local_midnight =
        (now_ms + local_offset_ms).div_euclid(MS_PER_DAY) * MS_PER_DAY - local_offset_ms;
    Ok(local_midnight + (h * 3_600_000 + m * 60_000))
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

    for ch in text.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let n: i64 = number
            .parse()
            .map_err(|_| ParseError::ExpectedNumber(text.clone(), ch))?;
        number.clear();
        total += match ch {
            'h' => n * 3_600_000,
            'm' => n * 60_000,
            's' => n * 1_000,
            _ => return Err(ParseError::UnknownUnit(text.clone(), ch)),
        };
        saw_unit = true;
    }

    if !number.is_empty() {
        let n: i64 = number
            .parse()
            .map_err(|_| ParseError::BadTrailingNumber(text.clone()))?;
        total += n * 60_000;
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

/// This entry's local wall-clock time as `HH:MM`, for prefilling an edit form.
///
/// The value parses back through [`parse_moment`] on the same local day, so a
/// dialog can show "keep 09:30" and mean it — provided the entry is from
/// today. Callers must only use this for entries whose local day is today;
/// otherwise `HH:MM` would silently move the endpoint to today.
pub fn format_local_hm(utc_ms: i64, local_offset_ms: i64) -> String {
    let rem = (utc_ms + local_offset_ms).rem_euclid(MS_PER_DAY);
    format!("{:02}:{:02}", rem / 3_600_000, (rem % 3_600_000) / 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_moment("now", "end", 1_000, 0).unwrap(), 1_000);
        assert_eq!(parse_moment("NOW", "end", 1_000, 0).unwrap(), 1_000);
        assert_eq!(parse_moment("  now  ", "end", 1_000, 0).unwrap(), 1_000);
    }

    #[test]
    fn minutes_ago_parses_relative_to_now() {
        let t = parse_moment("-90", "start", 10_000_000, 0).unwrap();
        assert_eq!(t, 10_000_000 - 5_400_000);
    }

    #[test]
    fn minutes_from_now_parses_forward() {
        assert_eq!(
            parse_moment("30", "start", 10_000_000, 0).unwrap(),
            11_800_000
        );
    }

    #[test]
    fn a_wall_clock_time_lands_today() {
        // now = 12:00 UTC on day 1; 09:30 must fall inside that UTC day.
        let noon_day1 = MS_PER_DAY + 12 * 3_600_000;
        let t = parse_moment("09:30", "start", noon_day1, 0).unwrap();
        assert_eq!(t, MS_PER_DAY + 9 * 3_600_000 + 30 * 60_000);
    }

    #[test]
    fn a_wall_clock_time_is_local_not_utc() {
        // UTC+2: anchoring on UTC midnight would put 00:30 two hours out.
        let off = 2 * 3_600_000;
        let now = MS_PER_DAY + 12 * 3_600_000; // 12:00 UTC, 14:00 local
        let t = parse_moment("00:30", "start", now, off).unwrap();
        assert_eq!((t + off).rem_euclid(MS_PER_DAY), 30 * 60_000);
        assert_eq!(
            (t + off).div_euclid(MS_PER_DAY),
            (now + off).div_euclid(MS_PER_DAY)
        );
    }

    #[test]
    fn impossible_times_are_refused() {
        assert!(parse_moment("25:00", "start", 0, 0).is_err());
        assert!(parse_moment("09:70", "start", 0, 0).is_err());
        assert!(parse_moment("nonsense", "start", 0, 0).is_err());
        assert!(parse_moment("", "start", 0, 0).is_err());
        assert!(parse_moment("-soon", "start", 0, 0).is_err());
    }

    #[test]
    fn the_error_names_the_field() {
        assert!(
            parse_moment("99:99", "end", 0, 0)
                .unwrap_err()
                .to_string()
                .contains("end")
        );
    }

    #[test]
    fn hm_formats_midnight_and_round_trips() {
        assert_eq!(format_local_hm(0, 0), "00:00");
        assert_eq!(format_local_hm(0, 3_600_000), "01:00");
        // 09:30 UTC parses back to itself on the same day.
        let t = 9 * 3_600_000 + 30 * 60_000;
        assert_eq!(format_local_hm(t, 0), "09:30");
        assert_eq!(
            parse_moment(&format_local_hm(t, 0), "start", t, 0).unwrap(),
            t
        );
    }
}
