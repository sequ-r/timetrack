/* core/model.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The persisted data model.
//!
//! Every entry is closed: the running timer was cut (REQUIREMENTS §7), so
//! `ended_at` is a plain `i64` and there is no such thing as a "running"
//! entry. The shape follows REQUIREMENTS §3.

use serde::{Deserialize, Serialize};

/// How an entry came to exist. Kept so hand-entered time is distinguishable
/// in an export (REQUIREMENTS §3, §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntrySource {
    /// Typed in, with either an explicit interval or a duration.
    Manual,
    /// A single tap on one of the Home tab's quick-add buttons.
    QuickAdd,
}

/// A single tracked interval.
///
/// `ended_at` is never `None`: entries are closed on creation. Entries may
/// overlap freely — overlap is a reporting concern, not an error
/// (REQUIREMENTS §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Identifier, unique within a store.
    pub id: String,
    /// Owning project. Grouping is a first-class part of the model
    /// (REQUIREMENTS §2), so this is a hard reference rather than a label.
    pub project_id: String,
    /// Optional note. No longer the label — that job belongs to the project.
    pub description: String,
    /// Start of the interval, milliseconds since the Unix epoch.
    pub started_at: i64,
    /// End of the interval, same units. Always at or after `started_at`.
    pub ended_at: i64,
    /// How this entry was recorded.
    pub source: EntrySource,
    /// Free-form note, never parsed or interpreted.
    #[serde(default)]
    pub note: Option<String>,
}

impl Entry {
    /// How long this entry ran, in milliseconds.
    ///
    /// Takes no `now`: every entry is closed, so there is nothing to
    /// interpolate. Clamped at zero so a bad interval cannot produce a
    /// negative total.
    pub fn duration_ms(&self) -> i64 {
        (self.ended_at - self.started_at).max(0)
    }

    /// This entry's local-time day, given the zone's offset at that instant.
    pub fn local_day(&self, local_offset_ms: i64) -> i64 {
        local_day_of(self.started_at, local_offset_ms)
    }
}

/// A named group of entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Identifier, unique within a store.
    pub id: String,
    /// Display name. Need not be unique; the id is what is referenced.
    pub name: String,
    /// Optional accent colour as 0xRRGGBB, used by both UIs.
    #[serde(default)]
    pub colour: Option<u32>,
    /// Archived projects keep their entries and stay in historical totals
    /// (REQUIREMENTS §14) — archiving hides them from pickers, it never
    /// rewrites history.
    #[serde(default)]
    pub archived: bool,
}

/// The whole persisted state, written as one JSON document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// Format version, so a future migration has something to branch on.
    /// We start clean (REQUIREMENTS §2), so there is no migration yet.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// The version this build writes. Bump when the shape changes.
pub const STORE_VERSION: u32 = 1;

impl Store {
    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Mutable access to an entry by id.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Look up a project by id.
    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Total of every entry, in milliseconds (sum of durations, §4).
    pub fn total_ms(&self) -> i64 {
        self.entries.iter().map(|e| e.duration_ms()).sum()
    }

    /// Sum of durations for one project.
    pub fn total_for_project(&self, project_id: &str) -> i64 {
        self.entries
            .iter()
            .filter(|e| e.project_id == project_id)
            .map(|e| e.duration_ms())
            .sum()
    }

    /// Entries most recent first, which is the order both UIs show.
    pub fn recent(&self) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = self.entries.iter().collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        out
    }

    /// Whether this store holds data written by a version we understand.
    ///
    /// Starting clean means the only expected state is empty or current, but
    /// the check exists so a future version can say "too new" rather than
    /// silently misreading a field.
    pub fn is_readable(&self) -> bool {
        self.version <= STORE_VERSION
    }
}

/// Local-time helpers.
///
/// Day and week bucketing is the single most error-prone part of this app
/// (see PLAN.md), so it lives in one place and takes the UTC offset as an
/// explicit parameter rather than reading a clock or a timezone database. The
/// service resolves the offset; the core stays pure and testable.
pub const MS_PER_DAY: i64 = 86_400_000;

/// The local-time day index of a UTC instant, given the zone's offset there.
///
/// Floor-divides, so instants before the epoch bucket correctly instead of
/// truncating towards zero and drifting a day.
pub fn local_day_of(utc_ms: i64, local_offset_ms: i64) -> i64 {
    // The offset is ADDED: a UTC+1 zone's local clock is ahead of UTC, so a
    // given instant has a later local time and can already be on the next
    // local day. Subtracting here pushed late-evening instants back a day.
    (utc_ms + local_offset_ms).div_euclid(MS_PER_DAY)
}

/// Milliseconds since the Unix epoch of local midnight opening `day`.
pub fn local_day_start(day: i64, local_offset_ms: i64) -> i64 {
    // Inverse of `local_day_of`, so the sign matches: the offset is added in
    // one and subtracted in the other.
    day * MS_PER_DAY - local_offset_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, start: i64, end: i64) -> Entry {
        Entry {
            id: id.into(),
            project_id: "p1".into(),
            description: String::new(),
            started_at: start,
            ended_at: end,
            source: EntrySource::Manual,
            note: None,
        }
    }

    #[test]
    fn duration_is_end_minus_start() {
        assert_eq!(entry("a", 1_000, 4_500).duration_ms(), 3_500);
    }

    #[test]
    fn duration_never_goes_negative() {
        // A bad interval must not produce a negative total.
        assert_eq!(entry("a", 5_000, 1_000).duration_ms(), 0);
    }

    #[test]
    fn total_sums_durations_not_wall_clock() {
        // Two overlapping hours are 2h summed (REQUIREMENTS §4), even though
        // they occupy only one hour of the day.
        let mut s = Store::default();
        s.entries.push(entry("a", 0, MS_PER_DAY));
        s.entries
            .push(entry("b", MS_PER_DAY / 2, MS_PER_DAY + MS_PER_DAY / 2));
        assert_eq!(s.total_ms(), MS_PER_DAY * 2);
    }

    #[test]
    fn total_per_project_ignores_others() {
        let mut s = Store::default();
        s.entries.push(entry("a", 0, 100));
        s.entries.push(Entry {
            project_id: "p2".into(),
            ..entry("b", 0, 500)
        });
        assert_eq!(s.total_for_project("p1"), 100);
        assert_eq!(s.total_for_project("p2"), 500);
    }

    #[test]
    fn recent_is_most_recent_first() {
        let mut s = Store::default();
        s.entries.push(entry("old", 10, 20));
        s.entries.push(entry("new", 300, 400));
        s.entries.push(entry("mid", 100, 200));
        let ids: Vec<&str> = s.recent().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut s = Store {
            version: STORE_VERSION,
            ..Default::default()
        };
        s.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: Some(0x2ea043),
            archived: false,
        });
        s.entries.push(entry("a", 1, 2));
        let text = serde_json::to_string(&s).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn empty_json_document_loads() {
        let s: Store = serde_json::from_str("{}").unwrap();
        assert!(s.entries.is_empty());
        assert!(s.projects.is_empty());
    }

    #[test]
    fn a_future_store_version_is_not_readable() {
        let s = Store {
            version: STORE_VERSION + 1,
            ..Default::default()
        };
        assert!(!s.is_readable(), "must refuse rather than misread fields");
    }

    // --- local-time bucketing -------------------------------------------------

    #[test]
    fn local_day_of_utc_midnight_is_day_zero() {
        assert_eq!(local_day_of(0, 0), 0);
    }

    #[test]
    fn local_day_respects_a_positive_offset() {
        // 23:30 UTC is already tomorrow in UTC+1.
        let t = MS_PER_DAY - 1_800_000;
        assert_eq!(local_day_of(t, 3_600_000), 1);
    }

    #[test]
    fn local_day_respects_a_negative_offset() {
        // 00:00 UTC on day 1 is 23:00 on day 0 in UTC-1, so it buckets as day 0.
        assert_eq!(local_day_of(MS_PER_DAY, -3_600_000), 0);
        // One second later it is 23:00:01 -- still day 0.
        assert_eq!(local_day_of(MS_PER_DAY + 1000, -3_600_000), 0);
        // And 01:00 UTC is 00:00 on day 1 locally, so it rolls over.
        assert_eq!(local_day_of(MS_PER_DAY + 3_600_000, -3_600_000), 1);
    }

    #[test]
    fn local_day_handles_instants_before_the_epoch() {
        // Floor towards negative infinity, or day indices drift around the
        // epoch.
        assert_eq!(local_day_of(-1, 0), -1);
        assert_eq!(local_day_of(-MS_PER_DAY, 0), -1);
    }

    #[test]
    fn local_day_start_round_trips_through_local_day_of() {
        for day in [-3_i64, 0, 1, 400] {
            let start = local_day_start(day, 3_600_000);
            assert_eq!(local_day_of(start, 3_600_000), day);
        }
    }

    #[test]
    fn local_day_of_is_the_inverse_of_day_start() {
        for offset in [0, 3_600_000, -3_600_000, 5 * 3_600_000] {
            for day in [-5_i64, 0, 1, 999] {
                assert_eq!(local_day_of(local_day_start(day, offset), offset), day);
            }
        }
    }
}
