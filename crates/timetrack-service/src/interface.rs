/* service/interface.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The D-Bus interface implementation.
//!
//! All state lives behind a single `Mutex<Inner>`, and every mutating method
//! persists before it returns. That ordering matters: a client that is told
//! "started" must be able to rely on the entry surviving a crash.

use timetrack_proto::{self as protocol, EntryView, Snapshot};
use std::sync::Mutex;
use timetrack_core::{
    cancel_entry, delete_entry, now_ms, set_description, start_entry, stop_entry, CounterIds,
    JsonStore, Store,
};

struct Inner {
    store: JsonStore,
    data: Store,
    ids: CounterIds,
}

/// Serialise access to the store.
pub struct TimerService {
    inner: Mutex<Inner>,
}

impl TimerService {
    pub fn new(store: JsonStore) -> anyhow::Result<Self> {
        let data = store.load()?;
        // Continue the id sequence past anything already on disk, so a restart
        // cannot mint an id that collides with a stored entry.
        let next = data
            .entries
            .iter()
            .filter_map(|e| e.id.strip_prefix('e'))
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        Ok(TimerService {
            inner: Mutex::new(Inner {
                store,
                data,
                ids: CounterIds::new(next),
            }),
        })
    }

    /// Run `f` under the lock and persist the result.
    fn mutate<T>(&self, f: impl FnOnce(&mut Inner) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut guard = self.inner.lock().expect("service state mutex poisoned");
        let out = f(&mut guard)?;
        guard.store.save(&guard.data)?;
        Ok(out)
    }

    /// Build a point-in-time view of the current state.
    fn snapshot_of(data: &Store) -> Snapshot {
        let now = now_ms();
        Snapshot {
            running: data.running().map(protocol::to_view),
            entries: data.recent().into_iter().map(protocol::to_view).collect(),
            total_ms: data.total_ms(now),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let guard = self.inner.lock().expect("service state mutex poisoned");
        Self::snapshot_of(&guard.data)
    }

    pub fn start(&self, description: &str) -> anyhow::Result<EntryView> {
        self.mutate(|i| {
            let e = start_entry(&mut i.data, &mut i.ids, description, now_ms())?;
            Ok(protocol::to_view(&e))
        })
    }

    pub fn stop(&self) -> anyhow::Result<EntryView> {
        self.mutate(|i| {
            let e = stop_entry(&mut i.data, now_ms())?;
            Ok(protocol::to_view(&e))
        })
    }

    pub fn cancel(&self) -> anyhow::Result<EntryView> {
        self.mutate(|i| {
            let e = cancel_entry(&mut i.data)?;
            Ok(protocol::to_view(&e))
        })
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        self.mutate(|i| {
            delete_entry(&mut i.data, id)?;
            Ok(())
        })
    }

    pub fn rename(&self, id: &str, description: &str) -> anyhow::Result<EntryView> {
        self.mutate(|i| {
            let e = set_description(&mut i.data, id, description)?;
            Ok(protocol::to_view(&e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn service(name: &str) -> (TimerService, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tt-svc-{name}"));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        (TimerService::new(JsonStore::new(&path)).unwrap(), path)
    }

    #[test]
    fn start_then_stop_persists_across_restart() {
        let (svc, path) = service("restart");
        svc.start("writing the report").unwrap();
        svc.stop().unwrap();

        // A fresh service over the same file must see the entry.
        let reopened = TimerService::new(JsonStore::new(&path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].description, "writing the report");
        assert!(!snap.entries[0].is_running());
    }

    #[test]
    fn running_entry_survives_restart_as_running() {
        // The service may be restarted while a timer is in flight; the entry
        // must not silently become a completed one.
        let (svc, path) = service("still-running");
        svc.start("long job").unwrap();

        let reopened = TimerService::new(JsonStore::new(&path)).unwrap();
        let snap = reopened.snapshot();
        assert!(snap.running.is_some());
        assert_eq!(snap.running.unwrap().description, "long job");
    }

    #[test]
    fn ids_do_not_collide_after_restart() {
        let (svc, path) = service("ids");
        for i in 0..3 {
            svc.start(&format!("t{i}")).unwrap();
            svc.stop().unwrap();
        }
        let reopened = TimerService::new(JsonStore::new(&path)).unwrap();
        let started = reopened.start("after restart").unwrap();

        let ids: Vec<String> = reopened
            .snapshot()
            .entries
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(
            ids.iter().filter(|id| **id == started.id).count(),
            1,
            "id {} collided with an existing entry: {ids:?}",
            started.id
        );
    }

    #[test]
    fn double_start_is_an_error() {
        let (svc, _) = service("double");
        svc.start("a").unwrap();
        assert!(svc.start("b").is_err());
    }

    #[test]
    fn stop_without_start_is_an_error() {
        let (svc, _) = service("nostop");
        assert!(svc.stop().is_err());
    }

    #[test]
    fn snapshot_reports_running_and_total() {
        let (svc, _) = service("snap");
        svc.start("a").unwrap();
        let snap = svc.snapshot();
        assert!(snap.running.is_some());
        assert_eq!(snap.entries.len(), 1);
        assert!(snap.total_ms >= 0);
    }

    #[test]
    fn remove_deletes_finished_entry() {
        let (svc, _) = service("remove");
        let e = svc.start("a").unwrap();
        svc.stop().unwrap();
        svc.remove(&e.id).unwrap();
        assert!(svc.snapshot().entries.is_empty());
    }

    #[test]
    fn remove_unknown_id_is_an_error() {
        let (svc, _) = service("remove-unknown");
        assert!(svc.remove("nope").is_err());
    }

    #[test]
    fn rename_updates_description() {
        let (svc, _) = service("rename");
        let e = svc.start("draft").unwrap();
        let updated = svc.rename(&e.id, "final").unwrap();
        assert_eq!(updated.description, "final");
    }

    #[test]
    fn cancel_leaves_store_empty_and_persisted() {
        let (svc, path) = service("cancel");
        svc.start("oops").unwrap();
        svc.cancel().unwrap();
        let reopened = TimerService::new(JsonStore::new(&path)).unwrap();
        assert!(reopened.snapshot().entries.is_empty());
    }
}
