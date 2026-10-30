/* core/tz.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Local timezone handling (REQUIREMENTS §9).
//!
//! Week and month bucketing is defined in *local* time (ISO weeks, calendar
//! months), so every instant must be mapped through the zone's UTC offset *at
//! that instant*. A fixed offset resolved once at startup is wrong for any
//! zone observing DST: an entry made after a transition buckets an hour out,
//! and a named `TZ` such as `Europe/Rome` is not a fixed offset at all.
//!
//! This module is the single place that knows about the tz database. It takes
//! no clock and reads the environment only in [`Tz::try_from_env`]; everything
//! else is pure over explicit instants, so it stays testable like the rest of
//! the core.
//!
//! # Who owns the zone (#102, ADR 0009)
//!
//! The service is the single zone authority: it resolves the zone with
//! [`Tz::try_from_env`], reports its name on every snapshot, and clients may
//! only build a `Tz` from that name ([`Tz::from_snapshot`]). Resolution is
//! strict — an unusable zone is an explicit [`TzError`], never a silent
//! fixed-offset or UTC fallback, because a guessed zone buckets entries into
//! the wrong local day while looking exactly like success.

use crate::model::MS_PER_DAY;

/// Why a timezone could not be resolved (#102).
///
/// There is deliberately no fallback variant: a caller that cannot name the
/// zone must fail loudly (the service at startup, a client when it would
/// parse a typed local time), not guess UTC or a fixed offset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TzError {
    /// `TZ` was set but names nothing usable.
    #[error(
        "TZ='{0}' is not a usable timezone (use an IANA name such as Europe/Rome, UTC, or UTC+2)"
    )]
    BadTzVar(String),
    /// The system zone named something unusable.
    #[error("the system timezone '{0}' is not usable (set TZ to an IANA name such as Europe/Rome)")]
    BadSystemZone(String),
    /// No zone could be determined at all.
    #[error("could not determine the local timezone (set TZ to an IANA name such as Europe/Rome)")]
    Undetermined,
    /// A service snapshot carried no zone name: an older service, which new
    /// clients must refuse rather than fall back to its fixed offset.
    #[error(
        "the service did not report a timezone name; client and service are different versions"
    )]
    MissingSnapshotZone,
    /// A service snapshot carried a zone name this client cannot parse.
    #[error("the service reported an unusable timezone '{0}'")]
    BadSnapshotZone(String),
}

/// A local timezone: UTC, a fixed offset, or a named IANA zone.
///
/// Fixed offsets cover the unambiguous `TZ=UTC±…` spellings. Named zones go
/// through the bundled tz database (`chrono-tz`), so the offset is resolved
/// per instant and DST transitions bucket correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tz {
    /// UTC itself.
    Utc,
    /// A fixed offset east of UTC, in milliseconds. The label is the spelling
    /// it parsed from (e.g. `UTC+05:30`), kept so [`Tz::name`] round-trips.
    Fixed {
        /// Offset east of UTC, in milliseconds.
        offset_ms: i64,
        /// Original spelling, for [`Tz::name`].
        label: String,
    },
    /// An IANA zone such as `Europe/Rome`.
    Named {
        /// IANA name, for [`Tz::name`].
        name: String,
        /// The database entry.
        inner: chrono_tz::Tz,
    },
}

impl Tz {
    /// UTC.
    pub fn utc() -> Self {
        Tz::Utc
    }

    /// A fixed offset east of UTC, in milliseconds.
    pub fn fixed_ms(offset_ms: i64) -> Self {
        let label = format_fixed_label(offset_ms);
        Tz::Fixed { offset_ms, label }
    }

    /// Parse a `TZ` value: `UTC`, `UTC±HH[:MM]`, `UTC±HHMM`, or an IANA name.
    /// A leading `:` (the POSIX implementation-defined prefix) is accepted.
    ///
    /// Returns `None` rather than guessing: mis-parsing a zone puts entries
    /// in the wrong local day, which is worse than failing.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().strip_prefix(':').unwrap_or(s.trim());
        if s.is_empty() {
            return None;
        }
        if s.eq_ignore_ascii_case("utc") {
            return Some(Tz::Utc);
        }
        if let Some(offset_ms) = parse_fixed_tz(s) {
            if offset_ms == 0 {
                return Some(Tz::Utc);
            }
            return Some(Tz::Fixed {
                offset_ms,
                label: s.to_string(),
            });
        }
        // Named zone via the tz database. chrono-tz parses exact IANA names;
        // try the spelling as given first.
        if let Ok(inner) = s.parse::<chrono_tz::Tz>() {
            return Some(Tz::Named {
                name: s.to_string(),
                inner,
            });
        }
        None
    }

    /// Resolve the local zone from the environment, strictly (#102).
    ///
    /// `TZ` wins when set; otherwise the system timezone. An unusable zone
    /// is an error, never a silent UTC fallback: the service resolves its
    /// zone this way at startup, so a typo'd `TZ` stops it with a message
    /// the user can act on instead of bucketing a day wrong.
    pub fn try_from_env() -> Result<Self, TzError> {
        let tz_var = std::env::var("TZ").ok();
        if tz_var.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            return Self::resolve(tz_var.as_deref(), None);
        }
        Self::resolve(None, iana_time_zone::get_timezone().ok().as_deref())
    }

    /// The pure half of [`Tz::try_from_env`], so both sources are testable
    /// without mutating the process environment.
    fn resolve(tz_var: Option<&str>, system_zone: Option<&str>) -> Result<Self, TzError> {
        if let Some(value) = tz_var {
            let value = value.trim();
            if !value.is_empty() {
                return Tz::parse(value).ok_or_else(|| TzError::BadTzVar(value.to_string()));
            }
        }
        match system_zone {
            Some(name) if !name.trim().is_empty() => Tz::parse(name.trim())
                .ok_or_else(|| TzError::BadSystemZone(name.trim().to_string())),
            _ => Err(TzError::Undetermined),
        }
    }

    /// Build from a snapshot's zone name, strictly (#102).
    ///
    /// The service always sends a parseable [`Tz::name`]. Anything else —
    /// an empty name from an older service, a name this client cannot
    /// parse — is an error the client must surface, never a fall back to
    /// the snapshot's fixed `local_offset_ms`, which can bucket a typed
    /// local time differently from the service's totals across DST.
    pub fn from_snapshot(tz_name: &str) -> Result<Self, TzError> {
        let name = tz_name.trim();
        if name.is_empty() {
            return Err(TzError::MissingSnapshotZone);
        }
        Tz::parse(name).ok_or_else(|| TzError::BadSnapshotZone(name.to_string()))
    }

    /// A parseable name for this zone: `UTC`, the fixed-offset spelling, or
    /// the IANA name. [`Tz::parse`] round-trips it.
    pub fn name(&self) -> &str {
        match self {
            Tz::Utc => "UTC",
            Tz::Fixed { label, .. } => label.as_str(),
            Tz::Named { name, .. } => name.as_str(),
        }
    }

    /// The zone's offset east of UTC at `utc_ms`, in milliseconds.
    pub fn offset_at_ms(&self, utc_ms: i64) -> i64 {
        match self {
            Tz::Utc => 0,
            Tz::Fixed { offset_ms, .. } => *offset_ms,
            Tz::Named { inner, .. } => offset_for_named(*inner, utc_ms),
        }
    }

    /// The local-time day index of a UTC instant, using the offset in force
    /// *at that instant*.
    ///
    /// Floor-divides, so pre-epoch instants bucket correctly.
    pub fn day_of(&self, utc_ms: i64) -> i64 {
        // Saturating (#98): an extreme wire instant buckets at the range
        // edge (where the calendar conversions clamp) instead of wrapping.
        utc_ms
            .saturating_add(self.offset_at_ms(utc_ms))
            .div_euclid(MS_PER_DAY)
    }

    /// Milliseconds since the epoch of local midnight opening `day`.
    ///
    /// The inverse of [`Tz::day_of`]. For named zones the offset at midnight
    /// itself decides the answer, so this iterates to a fixed point rather
    /// than assuming one offset: a midnight on either side of a transition
    /// needs the offset in force *there*, not the offset at some other
    /// instant. Converges in at most two steps — offsets only change at
    /// transitions, which are hours away from most midnights.
    pub fn day_start(&self, day: i64) -> i64 {
        // Saturating throughout (#98): an out-of-range day saturates to the
        // nearest representable midnight instead of wrapping.
        let day_ms = day.saturating_mul(MS_PER_DAY);
        match self {
            Tz::Utc => day_ms,
            Tz::Fixed { offset_ms, .. } => day_ms.saturating_sub(*offset_ms),
            Tz::Named { .. } => {
                let mut utc = day_ms.saturating_sub(self.offset_at_ms(day_ms));
                for _ in 0..4 {
                    let next = day_ms.saturating_sub(self.offset_at_ms(utc));
                    if next == utc {
                        break;
                    }
                    utc = next;
                }
                utc
            }
        }
    }
}

/// Format a fixed offset back into a `UTC±…` label.
fn format_fixed_label(offset_ms: i64) -> String {
    if offset_ms == 0 {
        return "UTC".to_string();
    }
    let sign = if offset_ms < 0 { '-' } else { '+' };
    // `saturating_abs`: a rogue snapshot offset of `i64::MIN` must not panic
    // the label formatter (#98). Real offsets are within a day.
    let abs = offset_ms.saturating_abs();
    let h = abs / 3_600_000;
    let m = (abs % 3_600_000) / 60_000;
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h:02}:{m:02}")
    }
}

/// Parse the unambiguous subset of `TZ`: `UTC+2`, `UTC-05:30`, `UTC+0530`.
///
/// Only fixed-offset zones. A named zone needs the tz database; returning
/// `None` for one is what keeps [`Tz::parse`] trying the IANA path instead.
fn parse_fixed_tz(tz: &str) -> Option<i64> {
    let rest = tz.strip_prefix("UTC").or_else(|| tz.strip_prefix("utc"))?;
    if rest.is_empty() {
        return Some(0);
    }
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1_i64, &rest[1..]),
        b'-' => (-1_i64, &rest[1..]),
        _ => return None,
    };
    let (h, m) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        None if digits.len() <= 2 => (digits.parse::<i64>().ok()?, 0),
        None => (
            digits[..2].parse::<i64>().ok()?,
            digits[2..].parse::<i64>().ok()?,
        ),
    };
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(sign * (h * 3_600_000 + m * 60_000))
}

/// Offset of a named zone at one instant, in milliseconds east of UTC.
fn offset_for_named(tz: chrono_tz::Tz, utc_ms: i64) -> i64 {
    use chrono::{DateTime, TimeZone as _, Utc};
    use chrono_tz::OffsetComponents;

    let secs = utc_ms.div_euclid(1000);
    let millis = utc_ms.rem_euclid(1000) as u32;
    let Some(dt) = DateTime::<Utc>::from_timestamp(secs, millis * 1_000_000) else {
        return 0;
    };
    let naive = dt.naive_utc();
    let offset = tz.offset_from_utc_datetime(&naive);
    offset.base_utc_offset().num_seconds() * 1000 + offset.dst_offset().num_seconds() * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_parses_and_is_zero() {
        assert_eq!(Tz::parse("UTC"), Some(Tz::Utc));
        assert_eq!(Tz::parse("utc"), Some(Tz::Utc));
        assert_eq!(Tz::utc().offset_at_ms(0), 0);
        assert_eq!(Tz::utc().offset_at_ms(1_700_000_000_000), 0);
    }

    #[test]
    fn fixed_offsets_parse() {
        assert_eq!(
            Tz::parse("UTC+2"),
            Some(Tz::Fixed {
                offset_ms: 7_200_000,
                label: "UTC+2".into()
            })
        );
        assert_eq!(
            Tz::parse("UTC+05:30").map(|t| t.offset_at_ms(0)),
            Some(19_800_000)
        );
        assert_eq!(
            Tz::parse("UTC-5").map(|t| t.offset_at_ms(0)),
            Some(-18_000_000)
        );
    }

    #[test]
    fn a_named_zone_parses_and_round_trips() {
        let tz = Tz::parse("Europe/Rome").expect("Europe/Rome must parse");
        assert_eq!(tz.name(), "Europe/Rome");
        assert_eq!(Tz::parse(tz.name()), Some(tz));
    }

    #[test]
    fn unknown_zones_are_refused() {
        // Short abbreviations that are real tz-database zones (like CET) now
        // parse via the database — that is the fix. Truly unknown names and
        // out-of-range offsets are still refused rather than mis-parsed.
        let cet = Tz::parse("CET").expect("CET is a real tz-database zone");
        assert_eq!(cet.offset_at_ms(0), 3_600_000);
        assert_eq!(Tz::parse("Europe/Romeo"), None);
        assert_eq!(Tz::parse("UTC+25"), None);
        assert_eq!(Tz::parse(""), None);
    }

    #[test]
    fn from_snapshot_uses_the_name() {
        let tz = Tz::from_snapshot("Europe/Rome").expect("a named zone resolves");
        assert_eq!(tz.name(), "Europe/Rome");
    }

    #[test]
    fn from_snapshot_refuses_missing_and_unusable_names() {
        // #102: no silent fixed-offset/UTC fallback. An old service that
        // sends no name, or a name this client cannot parse, is an error
        // the client surfaces — never a guess.
        assert_eq!(Tz::from_snapshot(""), Err(TzError::MissingSnapshotZone));
        assert_eq!(Tz::from_snapshot("  "), Err(TzError::MissingSnapshotZone));
        assert_eq!(
            Tz::from_snapshot("Europe/Romeo"),
            Err(TzError::BadSnapshotZone("Europe/Romeo".into()))
        );
    }

    #[test]
    fn resolution_is_strict_for_both_sources() {
        // A usable TZ wins over the system zone.
        assert_eq!(
            Tz::resolve(Some("Europe/Rome"), Some("UTC"))
                .unwrap()
                .name(),
            "Europe/Rome"
        );
        // A bad TZ is an explicit error, not UTC.
        assert_eq!(
            Tz::resolve(Some("Europe/Romeo"), Some("UTC")),
            Err(TzError::BadTzVar("Europe/Romeo".into()))
        );
        // No TZ: the system zone, strictly.
        assert_eq!(
            Tz::resolve(None, Some("Europe/Rome")).unwrap().name(),
            "Europe/Rome"
        );
        assert_eq!(
            Tz::resolve(Some(""), Some("Nowhere/Special")),
            Err(TzError::BadSystemZone("Nowhere/Special".into()))
        );
        // Nothing usable anywhere is an error, not a guess.
        assert_eq!(Tz::resolve(None, None), Err(TzError::Undetermined));
        // The POSIX `:` prefix is accepted rather than refused.
        assert_eq!(
            Tz::resolve(Some(":Europe/Rome"), None).unwrap().name(),
            "Europe/Rome"
        );
    }

    #[test]
    fn rome_observes_dst() {
        // Europe/Rome is CET (+1) in winter, CEST (+2) in summer.
        let rome = Tz::parse("Europe/Rome").unwrap();
        // 2026-01-15 12:00 UTC: winter, +1.
        let winter = 1_768_478_400_000;
        assert_eq!(rome.offset_at_ms(winter), 3_600_000);
        // 2026-07-15 12:00 UTC: summer, +2.
        let summer = 1_784_116_800_000;
        assert_eq!(rome.offset_at_ms(summer), 7_200_000);
    }

    #[test]
    fn day_start_round_trips_through_day_of_for_a_named_zone() {
        let rome = Tz::parse("Europe/Rome").unwrap();
        // Walk across both 2026 transitions: spring forward 29 Mar, fall back
        // 25 Oct. Every local midnight must map back to its own day.
        let start = rome.day_of(1_773_000_000_000); // ~Mar 2026
        let end = rome.day_of(1_793_000_000_000); // ~Oct 2026
        for day in (start..=end).step_by(7) {
            let midnight = rome.day_start(day);
            assert_eq!(
                rome.day_of(midnight),
                day,
                "midnight of day {day} must bucket as day {day}"
            );
            // One second before midnight is still yesterday.
            assert_eq!(rome.day_of(midnight - 1), day - 1);
        }
    }

    #[test]
    fn day_start_matches_the_fixed_formula_for_fixed_zones() {
        for offset in [0, 3_600_000, -3_600_000, 19_800_000] {
            let tz = Tz::fixed_ms(offset);
            for day in [-3, 0, 1, 400, 20_000] {
                assert_eq!(tz.day_start(day), day * MS_PER_DAY - offset);
                assert_eq!(tz.day_of(tz.day_start(day)), day);
            }
        }
    }
}
