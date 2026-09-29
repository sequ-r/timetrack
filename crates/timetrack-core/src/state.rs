/* core/state.rs
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

//! The timer state machine.
//!
//! Every mutation is a method that takes the current [`Store`] and the current
//! wall-clock time, and returns the new store or a [`CoreError`]. The machine
//! is deliberately free of I/O and of any notion of "now" other than the time
//! it is handed, so the whole thing is directly testable and the service layer
//! stays a thin shell around it.

use crate::model::{Entry, Store};

/// Errors the state machine can produce.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("an entry is already running; stop it first")]
    AlreadyRunning,
    #[error("no entry is running")]
    NotRunning,
    #[error("no entry with id '{0}'")]
    UnknownEntry(String),
}

/// Result alias for state-machine operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Monotonic-ish id source. Kept separate so tests can pin it.
pub trait IdSource {
    fn next_id(&mut self) -> String;
}

/// Generates ids from a counter plus the process start time, which keeps them
/// unique within a store without pulling in a uuid dependency.
#[derive(Debug)]
pub struct CounterIds {
    next: u64,
}

impl CounterIds {
    pub fn new(start: u64) -> Self {
        CounterIds { next: start }
    }
}

impl Default for CounterIds {
    fn default() -> Self {
        CounterIds { next: 1 }
    }
}

impl IdSource for CounterIds {
    fn next_id(&mut self) -> String {
        let id = format!("e{}", self.next);
        self.next += 1;
        id
    }
}

/// Start a new entry. Fails if something is already running.
pub fn start_entry(
    store: &mut Store,
    ids: &mut dyn IdSource,
    description: &str,
    now: i64,
) -> CoreResult<Entry> {
    if store.running().is_some() {
        return Err(CoreError::AlreadyRunning);
    }
    let entry = Entry {
        id: ids.next_id(),
        description: description.to_string(),
        started_at: now,
        ended_at: None,
    };
    store.entries.push(entry.clone());
    Ok(entry)
}

/// Stop the running entry, recording `now` as its end time.
pub fn stop_entry(store: &mut Store, now: i64) -> CoreResult<Entry> {
    let running = store.running_mut().ok_or(CoreError::NotRunning)?;
    running.ended_at = Some(now);
    let id = running.id.clone();
    store
        .get(&id)
        .cloned()
        .ok_or(CoreError::UnknownEntry(id))
}

/// Discard the running entry entirely, recording nothing.
pub fn cancel_entry(store: &mut Store) -> CoreResult<Entry> {
    let running = store.running().ok_or(CoreError::NotRunning)?.clone();
    store.entries.retain(|e| e.id != running.id);
    Ok(running)
}

/// Delete a finished entry by id. Refuses to delete a running one, since that
/// would leave the store in a state the user did not ask for.
pub fn delete_entry(store: &mut Store, id: &str) -> CoreResult<Entry> {
    let entry = store
        .get(id)
        .cloned()
        .ok_or_else(|| CoreError::UnknownEntry(id.to_string()))?;
    if entry.is_running() {
        return Err(CoreError::AlreadyRunning);
    }
    store.entries.retain(|e| e.id != id);
    Ok(entry)
}

/// Rename an entry's description.
pub fn set_description(store: &mut Store, id: &str, description: &str) -> CoreResult<Entry> {
    let target = store
        .entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| CoreError::UnknownEntry(id.to_string()))?;
    target.description = description.to_string();
    Ok(target.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreError;

    fn store() -> Store {
        Store::default()
    }

    #[test]
    fn start_creates_running_entry() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let e = start_entry(&mut s, &mut ids, "writing docs", 100).unwrap();
        assert!(e.is_running());
        assert_eq!(e.description, "writing docs");
        assert_eq!(s.running().map(|e| e.id.as_str()), Some(e.id.as_str()));
    }

    #[test]
    fn start_twice_is_rejected() {
        let mut s = store();
        let mut ids = CounterIds::default();
        start_entry(&mut s, &mut ids, "a", 100).unwrap();
        let err = start_entry(&mut s, &mut ids, "b", 200).unwrap_err();
        assert_eq!(err, CoreError::AlreadyRunning);
        // The rejected start must not have appended anything.
        assert_eq!(s.entries.len(), 1);
    }

    #[test]
    fn stop_closes_the_running_entry() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let started = start_entry(&mut s, &mut ids, "a", 100).unwrap();
        let stopped = stop_entry(&mut s, 500).unwrap();
        assert_eq!(stopped.id, started.id);
        assert_eq!(stopped.ended_at, Some(500));
        assert!(!stopped.is_running());
        assert!(s.running().is_none());
    }

    #[test]
    fn stop_without_start_is_rejected() {
        let mut s = store();
        assert_eq!(stop_entry(&mut s, 1).unwrap_err(), CoreError::NotRunning);
    }

    #[test]
    fn start_stop_start_works_in_sequence() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let a = start_entry(&mut s, &mut ids, "a", 0).unwrap();
        stop_entry(&mut s, 100).unwrap();
        let b = start_entry(&mut s, &mut ids, "b", 200).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.running().map(|e| e.id.as_str()), Some(b.id.as_str()));
    }

    #[test]
    fn cancel_removes_the_entry_entirely() {
        let mut s = store();
        let mut ids = CounterIds::default();
        start_entry(&mut s, &mut ids, "a", 0).unwrap();
        cancel_entry(&mut s).unwrap();
        assert!(s.entries.is_empty());
        assert!(s.running().is_none());
    }

    #[test]
    fn cancel_without_start_is_rejected() {
        assert_eq!(
            cancel_entry(&mut store()).unwrap_err(),
            CoreError::NotRunning
        );
    }

    #[test]
    fn delete_removes_finished_entry() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let e = start_entry(&mut s, &mut ids, "a", 0).unwrap();
        stop_entry(&mut s, 10).unwrap();
        delete_entry(&mut s, &e.id).unwrap();
        assert!(s.entries.is_empty());
    }

    #[test]
    fn delete_refuses_running_entry() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let e = start_entry(&mut s, &mut ids, "a", 0).unwrap();
        assert_eq!(
            delete_entry(&mut s, &e.id).unwrap_err(),
            CoreError::AlreadyRunning
        );
        assert_eq!(s.entries.len(), 1, "entry must survive the refusal");
    }

    #[test]
    fn delete_unknown_id_is_rejected() {
        assert!(matches!(
            delete_entry(&mut store(), "nope").unwrap_err(),
            CoreError::UnknownEntry(_)
        ));
    }

    #[test]
    fn set_description_updates_entry() {
        let mut s = store();
        let mut ids = CounterIds::default();
        let e = start_entry(&mut s, &mut ids, "draft", 0).unwrap();
        let updated = set_description(&mut s, &e.id, "final").unwrap();
        assert_eq!(updated.description, "final");
    }

    #[test]
    fn ids_are_unique_across_many_entries() {
        let mut s = store();
        let mut ids = CounterIds::default();
        for i in 0..20 {
            start_entry(&mut s, &mut ids, &format!("t{i}"), i).unwrap();
            stop_entry(&mut s, i + 1).unwrap();
        }
        let unique: std::collections::HashSet<_> = s.entries.iter().map(|e| &e.id).collect();
        assert_eq!(unique.len(), 20);
    }
}
