/* core/model.rs
 *
 * Copyright 2026 sequ
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The persisted data model: entries and the shape of stored state.

use serde::{Deserialize, Serialize};

/// A single tracked interval.
///
/// `ended_at` is `None` while the entry is still running. A running entry is
/// the one the service treats as "current", and there is at most one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Monotonic identifier, unique within a store. Also the D-Bus object path
    /// suffix, so it must not contain characters invalid in a path element.
    pub id: String,
    /// Free-form label the user attached to this interval.
    pub description: String,
    /// Start of the interval, milliseconds since the Unix epoch.
    pub started_at: i64,
    /// End of the interval in the same units, or `None` while still running.
    pub ended_at: Option<i64>,
}

impl Entry {
    /// Duration in milliseconds, using `now` while the entry is still running.
    pub fn duration_ms(&self, now: i64) -> i64 {
        let end = self.ended_at.unwrap_or(now);
        (end - self.started_at).max(0)
    }

    /// Whether this entry is the in-progress one.
    pub fn is_running(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// The full persisted state, written as a single JSON document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Store {
    /// The in-progress entry, if any.
    pub fn running(&self) -> Option<&Entry> {
        self.entries.iter().find(|e| e.is_running())
    }

    /// Mutable access to the in-progress entry, if any.
    pub fn running_mut(&mut self) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.is_running())
    }

    /// Entries ordered most-recent-first, which is the order every UI shows.
    pub fn recent(&self) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = self.entries.iter().collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    /// Total tracked time in milliseconds, counting the running entry to `now`.
    pub fn total_ms(&self, now: i64) -> i64 {
        self.entries.iter().map(|e| e.duration_ms(now)).sum()
    }

    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, started: i64, ended: Option<i64>) -> Entry {
        Entry {
            id: id.into(),
            description: String::new(),
            started_at: started,
            ended_at: ended,
        }
    }

    #[test]
    fn running_entry_is_found() {
        let store = Store {
            entries: vec![entry("a", 100, Some(200)), entry("b", 300, None)],
        };
        assert_eq!(store.running().map(|e| e.id.as_str()), Some("b"));
    }

    #[test]
    fn only_the_first_running_entry_wins() {
        // Defensive: the state machine prevents this, but a hand-edited or
        // corrupted store could contain two, and `running` must not panic.
        let store = Store {
            entries: vec![entry("a", 0, None), entry("b", 10, None)],
        };
        assert_eq!(store.running().map(|e| e.id.as_str()), Some("a"));
    }

    #[test]
    fn duration_counts_running_entry_up_to_now() {
        let e = entry("a", 1_000, None);
        assert_eq!(e.duration_ms(4_500), 3_500);
    }

    #[test]
    fn duration_never_goes_negative() {
        // A clock adjustment backwards must not produce a negative total.
        let e = entry("a", 5_000, None);
        assert_eq!(e.duration_ms(1_000), 0);
    }

    #[test]
    fn total_sums_every_entry() {
        let store = Store {
            entries: vec![
                entry("a", 0, Some(100)),
                entry("b", 200, Some(500)),
                entry("c", 1_000, None),
            ],
        };
        // 100 + 300 + (1500-1000) = 900
        assert_eq!(store.total_ms(1_500), 900);
    }

    #[test]
    fn recent_is_most_recent_first() {
        let store = Store {
            entries: vec![
                entry("old", 10, Some(20)),
                entry("new", 300, Some(400)),
                entry("mid", 100, Some(200)),
            ],
        };
        let ids: Vec<&str> = store.recent().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn store_round_trips_through_json() {
        let store = Store {
            entries: vec![entry("a", 1, Some(2)), entry("b", 3, None)],
        };
        let text = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        assert_eq!(store, back);
    }

    #[test]
    fn empty_json_document_loads() {
        // A brand new store, and any file that predates entries existing.
        let store: Store = serde_json::from_str("{}").unwrap();
        assert!(store.entries.is_empty());
        assert!(store.running().is_none());
    }
}
