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
//! "added" must be able to rely on the entry surviving a crash.
//!
//! # This layer holds no rules
//!
//! Every decision -- what a valid interval is, what merging does, when a day
//! is over 24h -- is made in `timetrack-core`. The service's only jobs are to
//! resolve the clock, resolve the timezone offset, call the right rule, and
//! persist. That split is what stops the GUI and the CLI from disagreeing.
//!
//! # The clock
//!
//! The core takes timestamps as parameters and reads no clock, so exactly one
//! function here reads the system clock. That makes every entry's `ended_at`
//! traceable to a single instant per call instead of being interpolated.

use std::sync::Mutex;

use timetrack_core as core;
use timetrack_core::{Counter, EntrySource, JsonStore, RuleError, Store};
use timetrack_proto::{
    self as protocol, EntryView, ProjectView, ServiceError, Snapshot, TotalsView,
};

/// The current time, in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        // Before 1970 is not reachable in practice, but a backwards clock
        // should not panic the service.
        .unwrap_or(0)
}

struct Inner {
    store: JsonStore,
    data: Store,
    ids: Counter,
    /// The local zone, resolved once at startup from `TZ` (or the system
    /// zone). Offsets are resolved per instant from this, so DST transitions
    /// bucket correctly (REQUIREMENTS §9).
    tz: core::Tz,
}

/// Serialise access to the store.
pub struct EntryService {
    inner: Mutex<Inner>,
}

impl EntryService {
    pub fn new(store: JsonStore) -> anyhow::Result<Self> {
        Self::new_with_tz(store, core::Tz::from_env())
    }

    /// Build with an explicit zone. The production path is [`EntryService::new`]
    /// (from the environment); this exists so tests can pin a named zone
    /// without mutating the process-global `TZ`.
    pub fn new_with_tz(store: JsonStore, tz: core::Tz) -> anyhow::Result<Self> {
        let mut data = store.load()?;
        // A brand-new store has no projects, and with no project there is
        // nothing to attribute time to -- which made the Home tab's quick-add
        // buttons do nothing at all on a fresh install. Seed one so the app
        // is usable the moment it opens. Only on a genuinely empty store: an
        // existing one is left exactly as it is, archived projects and all.
        let seeded = data.projects.is_empty();
        if seeded {
            data.projects.push(timetrack_core::Project {
                id: "p1".into(),
                name: "General".into(),
                colour: None,
                archived: false,
            });
        }
        let ids = Counter::continuing(&data);
        let service = EntryService {
            inner: Mutex::new(Inner {
                store,
                data,
                ids,
                tz,
            }),
        };
        // Persist the seed immediately, so a read-only store surfaces at
        // startup rather than on the user's first tap -- but only when there
        // was something to seed. Rewriting the file on every launch needlessly
        // dirties backups and turns a read-only store into a startup failure.
        if seeded {
            service.persist()?;
        }
        Ok(service)
    }

    /// Write the current state out. Used by the mutation funnel and by
    /// startup, so both go through the same path.
    fn persist(&self) -> anyhow::Result<()> {
        let g = self.inner.lock().expect("service state mutex poisoned");
        g.store.save(&g.data).map_err(anyhow::Error::from)
    }

    /// A consistent view of everything a client needs for one frame.
    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().expect("service state mutex poisoned");
        let now = now_ms();
        let today = g.tz.day_of(now);
        let week = core::week_totals(&g.data, today, &g.tz);
        let month = core::month_totals(&g.data, today, &g.tz);
        let all = core::all_totals(&g.data);

        Snapshot {
            projects: g
                .data
                .projects
                .iter()
                .map(protocol::project_to_view)
                .collect(),
            entries: g
                .data
                .recent()
                .into_iter()
                .map(protocol::entry_to_view)
                .collect(),
            week: to_totals_view(&week),
            month: to_totals_view(&month),
            all: to_totals_view(&all),
            total_ms: all.total_ms,
            over_24h_days: core::days_exceeding_24h(&g.data, &g.tz),
            local_offset_ms: g.tz.offset_at_ms(now),
            tz: g.tz.name().to_string(),
        }
    }

    /// Apply a mutation to the store, persist, and return the result.
    ///
    /// The single funnel every mutating method goes through, which is what
    /// guarantees the persist-before-acknowledge ordering. Failures keep
    /// their type: rule refusals stay `Rule`, persist failures become
    /// `Storage`, so the D-Bus layer can encode them without flattening.
    /// If the write fails the error is returned and the in-memory state is
    /// kept as is, so a failed save never discards tracked time: the mutation
    /// stays visible and a later successful save persists it along with
    /// everything else.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Store, &mut Counter, i64) -> core::RuleResult<T>,
    ) -> Result<T, ServiceError> {
        let mut g = self.inner.lock().expect("service state mutex poisoned");
        let now = now_ms();
        // Destructure so `data` and `ids` are borrowed separately; borrowing
        // two fields of the same struct mutably in one call is rejected.
        let Inner {
            data, ids, store, ..
        } = &mut *g;
        let result = f(data, ids, now);
        match result {
            Ok(value) => {
                // Persist before acknowledging. If the write fails, report
                // the error but keep the in-memory state: reloading here
                // would discard tracked time whenever the reload fails too,
                // and the next successful save heals a transient failure.
                if let Err(e) = store.save(data) {
                    return Err(ServiceError::Storage(e.to_string()));
                }
                Ok(value)
            }
            Err(rule) => Err(ServiceError::from(rule)),
        }
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    /// Method 1: an explicit start and end.
    pub fn add(
        &self,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, ids, _| {
            core::create_entry(s, ids, project_id, description, started_at, ended_at)
        })?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Method 2: a duration ending now.
    pub fn add_duration(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, ids, now| {
            core::create_duration(s, ids, project_id, description, duration_ms, now)
        })?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Method 3: a duration ending at a given instant.
    pub fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> Result<EntryView, ServiceError> {
        self.add(project_id, description, ended_at - duration_ms, ended_at)
    }

    /// Method 4: one of the quick-add buckets, ending now.
    pub fn quick_add(&self, project_id: &str, duration_ms: i64) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, ids, now| core::quick_add(s, ids, project_id, duration_ms, now))?;
        Ok(protocol::entry_to_view(&e))
    }

    // --- editing ---

    /// Shorten by moving an endpoint. Never deletes (REQUIREMENTS §5).
    pub fn set_times(
        &self,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, _ids, _| core::set_times(s, id, started_at, ended_at))?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Edit an entry's description (the manual-entry dialog's text field).
    pub fn set_text(&self, id: &str, description: &str) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, _ids, _| core::set_text(s, id, Some(description), None))?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Reassign an entry to another project.
    pub fn set_project(&self, id: &str, project_id: &str) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, _ids, _| core::set_project(s, id, project_id))?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Undo exactly the entry a quick-add created.
    ///
    /// Refuses anything not created by quick-add, so a client's "undo" can
    /// never delete hand-entered time by mistake.
    pub fn undo_quick_add(&self, id: &str) -> Result<(), ServiceError> {
        self.mutate(|s, _ids, _| {
            let entry = s
                .get(id)
                .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
            if entry.source != EntrySource::QuickAdd {
                return Err(RuleError::NotQuickAdd(id.to_string()));
            }
            core::delete_entry(s, id).map(|_| ())
        })
    }

    /// Explicit deletion.
    pub fn delete_entry(&self, id: &str) -> Result<(), ServiceError> {
        self.mutate(|s, _ids, _| core::delete_entry(s, id).map(|_| ()))
    }

    pub fn split(&self, id: &str, at_ms: i64) -> Result<(EntryView, EntryView), ServiceError> {
        let (a, b) = self.mutate(|s, ids, _| core::split_entry(s, ids, id, at_ms))?;
        Ok((protocol::entry_to_view(&a), protocol::entry_to_view(&b)))
    }

    /// Merge entries into one spanning the union of their intervals.
    pub fn merge(&self, ids: &[String]) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, _ids, _| core::merge_entries(s, ids))?;
        Ok(protocol::entry_to_view(&e))
    }

    // --- projects ---

    pub fn add_project(&self, name: &str) -> Result<ProjectView, ServiceError> {
        let p = self.mutate(|s, ids, _| core::create_project(s, ids, name))?;
        Ok(protocol::project_to_view(&p))
    }

    /// Rename a project, recolour it, or both.
    ///
    /// An empty `name` keeps the current name; a negative `colour` keeps the
    /// current colour, otherwise the low 24 bits become the new `0xRRGGBB`.
    /// (D-Bus has no `Option` on the wire, so "leave it alone" needs these
    /// sentinels; the core function underneath still takes real options.)
    pub fn update_project(
        &self,
        id: &str,
        name: &str,
        colour: i64,
    ) -> Result<ProjectView, ServiceError> {
        let name = if name.is_empty() { None } else { Some(name) };
        let colour = if colour < 0 {
            None
        } else {
            Some(Some((colour as u32) & 0x00FF_FFFF))
        };
        let p = self.mutate(|s, _ids, _| core::update_project(s, id, name, colour))?;
        Ok(protocol::project_to_view(&p))
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<ProjectView, ServiceError> {
        let p = self.mutate(|s, _ids, _| core::set_archived(s, id, archived))?;
        Ok(protocol::project_to_view(&p))
    }

    pub fn delete_project(&self, id: &str) -> Result<(), ServiceError> {
        self.mutate(|s, _ids, _| core::delete_project(s, id).map(|_| ()))
    }

    /// CSV export (REQUIREMENTS §10): one row per entry in `scope`, with a
    /// header, ISO 8601 local timestamps, `HH:MM:SS` + minutes, and `source`.
    pub fn export_csv(&self, scope: &str) -> Result<String, ServiceError> {
        let scope = core::ExportScope::parse(scope)
            .ok_or_else(|| ServiceError::UnknownExportScope(scope.to_string()))?;
        let g = self.inner.lock().expect("service state mutex poisoned");
        let now = now_ms();
        Ok(core::export_csv(&g.data, scope, now, &g.tz))
    }
}

fn to_totals_view(t: &core::Totals) -> TotalsView {
    TotalsView {
        total_ms: t.total_ms,
        per_project: t.per_project.clone(),
        entry_count: t.entry_count,
    }
}

/// Timezone resolution lives in `timetrack-core::Tz`: `Tz::from_env` reads
/// `TZ` (or the system zone), and offsets are then resolved per instant, so
/// DST transitions bucket correctly. There is deliberately no offset logic
/// left here — one implementation, in the core, so both UIs agree.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn service(name: &str) -> (EntryService, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tt-svc-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        (
            EntryService::new(JsonStore::new(path.clone())).unwrap(),
            path,
        )
    }

    fn project(svc: &EntryService) -> ProjectView {
        if let Some(p) = svc.snapshot().projects.first() {
            return p.clone();
        }
        svc.add_project("Work").unwrap()
    }

    // --- persistence ---

    #[test]
    fn an_entry_persists_across_restart() {
        let (svc, path) = service("restart");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "writing", now - 3_600_000, now).unwrap();

        // A fresh service over the same file must see the entry.
        let reopened = EntryService::new(JsonStore::new(path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].description, "writing");
    }

    #[test]
    fn ids_do_not_collide_after_restart() {
        let (svc, path) = service("ids");
        let p = project(&svc);
        let now = now_ms();
        for i in 0..3 {
            svc.add(&p.id, &format!("t{i}"), now, now + 1).unwrap();
        }
        let reopened = EntryService::new(JsonStore::new(path)).unwrap();
        let added = reopened.add(&p.id, "after restart", now, now + 1).unwrap();

        let ids: Vec<String> = reopened
            .snapshot()
            .entries
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(
            ids.iter().filter(|id| **id == added.id).count(),
            1,
            "id {} collided with an existing entry: {ids:?}",
            added.id
        );
    }

    #[test]
    fn reopening_an_existing_store_leaves_the_file_alone() {
        // Startup used to rewrite the file unconditionally, dirtying backups
        // and failing on read-only stores that could otherwise be served.
        let (svc, path) = service("untouched");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "kept", now, now + 1).unwrap();

        let before = std::fs::read(&path).unwrap();
        let _ = EntryService::new(JsonStore::new(path.clone())).unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "starting up must not rewrite the store");
    }

    #[test]
    fn a_failed_write_does_not_leave_the_service_reporting_success() {
        // The mutate funnel returns the save error, so a client never sees
        // an entry the service could not persist as acknowledged.
        let (svc, path) = service("failedwrite");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "kept", now, now + 1).unwrap();

        // Make the store unwritable by turning its path into a directory.
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(&path).unwrap();
        let err = svc.add(&p.id, "lost", now, now + 1);
        assert!(err.is_err(), "must not acknowledge an unpersisted entry");
    }

    #[test]
    fn a_failed_save_keeps_in_memory_state() {
        // The double failure: the save fails, and a reload would fail too
        // (the path is a directory). The service must keep serving what it
        // holds rather than resetting to an empty store.
        let (svc, path) = service("double-failure");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "kept", now, now + 1).unwrap();

        // Break persistence both ways by turning the store path into a
        // directory: saves fail, and any reload would fail as well.
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(&path).unwrap();
        let err = svc.add(&p.id, "unpersisted", now, now + 1);
        assert!(err.is_err(), "must not acknowledge an unpersisted entry");

        // Nothing was wiped: the earlier entry, the seed project and the
        // just-applied (unpersisted) entry are all still served.
        let snap = svc.snapshot();
        assert_eq!(snap.projects.len(), 1, "the seed project must survive");
        assert_eq!(snap.entries.len(), 2, "in-memory state must be kept");
        assert!(
            snap.entries.iter().any(|e| e.description == "kept"),
            "previously persisted data must survive: {snap:?}"
        );
        assert!(
            snap.entries.iter().any(|e| e.description == "unpersisted"),
            "the failed mutation stays in memory: {snap:?}"
        );

        // And the failure heals: once the path is writable again the next
        // mutation persists everything, including the entry from the failed
        // call, so a transient failure loses nothing.
        std::fs::remove_dir_all(&path).unwrap();
        svc.add(&p.id, "healed", now, now + 1).unwrap();
        let reopened = EntryService::new(JsonStore::new(path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.entries.len(), 3, "all three entries must be on disk");
    }

    // --- the four methods ---

    #[test]
    fn method_one_takes_an_explicit_interval() {
        let (svc, _) = service("m1");
        let p = project(&svc);
        let e = svc.add(&p.id, "explicit", 0, 3_600_000).unwrap();
        assert_eq!(e.duration_ms(), 3_600_000);
        assert_eq!(e.ended_at, 3_600_000);
    }

    #[test]
    fn method_two_ends_now() {
        let (svc, _) = service("m2");
        let p = project(&svc);
        let before = now_ms();
        let e = svc.add_duration(&p.id, "an hour", 3_600_000).unwrap();
        let after = now_ms();
        assert!(e.ended_at >= before && e.ended_at <= after);
        assert_eq!(e.duration_ms(), 3_600_000);
    }

    #[test]
    fn method_three_ends_in_the_past() {
        let (svc, _) = service("m3");
        let p = project(&svc);
        let e = svc
            .add_duration_ending(&p.id, "earlier", 3_600_000, 86_400_000)
            .unwrap();
        assert_eq!(e.ended_at, 86_400_000);
        assert_eq!(e.started_at, 86_400_000 - 3_600_000);
    }

    #[test]
    fn method_four_is_its_own_undoable_entry() {
        let (svc, _) = service("m4");
        let p = project(&svc);
        let a = svc.quick_add(&p.id, 15 * 60_000).unwrap();
        let b = svc.quick_add(&p.id, 15 * 60_000).unwrap();
        assert_ne!(a.id, b.id, "each tap is a separate entry");
        assert_eq!(svc.snapshot().entries.len(), 2);

        svc.undo_quick_add(&a.id).unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].id, b.id, "only the quick-added one went");
    }

    #[test]
    fn undo_refuses_a_hand_entered_entry() {
        let (svc, _) = service("undo-guard");
        let p = project(&svc);
        let e = svc.add(&p.id, "typed by hand", 0, 1000).unwrap();
        assert!(svc.undo_quick_add(&e.id).is_err());
        assert_eq!(svc.snapshot().entries.len(), 1, "nothing was deleted");
    }

    #[test]
    fn a_rejected_interval_stores_nothing() {
        let (svc, _) = service("rejected");
        let p = project(&svc);
        assert!(svc.add(&p.id, "backwards", 5_000, 1_000).is_err());
        assert!(svc.snapshot().entries.is_empty());
    }

    #[test]
    fn refusals_keep_their_type() {
        // The funnel must not flatten errors back to strings: the D-Bus
        // layer encodes the typed refusal, so clients can match on it.
        let (svc, _) = service("typed-refusal");
        let p = project(&svc);
        assert_eq!(
            svc.add(&p.id, "backwards", 5_000, 1_000).unwrap_err(),
            ServiceError::Rule(RuleError::NegativeDuration)
        );
        assert_eq!(
            svc.undo_quick_add("nope").unwrap_err(),
            ServiceError::Rule(RuleError::UnknownEntry("nope".into()))
        );
        assert_eq!(
            svc.export_csv("everything").unwrap_err(),
            ServiceError::UnknownExportScope("everything".into())
        );
    }

    // --- editing preserves data ---

    #[test]
    fn shortening_moves_an_endpoint_without_deleting() {
        let (svc, _) = service("shorten");
        let p = project(&svc);
        let e = svc.add(&p.id, "too long", 0, 3_600_000).unwrap();
        let shorter = svc.set_times(&e.id, 0, 1_800_000).unwrap();
        assert_eq!(shorter.duration_ms(), 1_800_000);
        assert_eq!(svc.snapshot().entries.len(), 1, "the entry is still there");
    }

    #[test]
    fn delete_is_explicit_and_removes_exactly_one() {
        let (svc, _) = service("delete");
        let p = project(&svc);
        let a = svc.quick_add(&p.id, 5 * 60_000).unwrap();
        let b = svc.quick_add(&p.id, 5 * 60_000).unwrap();
        svc.delete_entry(&a.id).unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].id, b.id);
    }

    #[test]
    fn deleting_an_unknown_id_is_an_error() {
        let (svc, _) = service("delete-unknown");
        assert!(svc.delete_entry("nope").is_err());
    }

    #[test]
    fn split_halves_get_distinct_ids() {
        // Pins the service wiring: `split` must mint the second half from the
        // id counter, so splitting twice cannot collide.
        let (svc, _) = service("splitids");
        let p = project(&svc);
        let now = now_ms();
        let e = svc.add(&p.id, "long", now - 3_600_000, now).unwrap();
        let (a, b) = svc.split(&e.id, now - 1_800_000).unwrap();
        assert_ne!(a.id, b.id);
        let (c, d) = svc.split(&a.id, now - 2_700_000).unwrap();
        assert_ne!(c.id, d.id);
        // The store itself must hold no id twice: the first half keeps its id
        // by design, so uniqueness is a property of the store, not of the
        // returned pairs.
        let snap = svc.snapshot();
        let ids: Vec<&str> = snap.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(id), "duplicate id {id}");
        }
    }

    #[test]
    fn merge_with_itself_is_an_error_not_a_panic() {
        let (svc, _) = service("mergeself");
        let p = project(&svc);
        let now = now_ms();
        let e = svc.add(&p.id, "one", now - 1_000, now).unwrap();
        let err = svc.merge(&[e.id.clone(), e.id.clone()]);
        assert!(err.is_err(), "must refuse, not panic");
        assert_eq!(svc.snapshot().entries.len(), 1);
    }

    // --- aggregation lands in the service ---

    #[test]
    fn the_snapshot_carries_week_and_month_totals() {
        let (svc, _) = service("totals");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "this week", now - 3_600_000, now).unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.week.total_ms, 3_600_000);
        assert_eq!(snap.month.total_ms, 3_600_000);
        assert_eq!(snap.total_ms, 3_600_000);
        assert_eq!(snap.week.per_project.get(&p.id), Some(&3_600_000));
    }

    #[test]
    fn the_snapshot_carries_all_time_totals_per_project() {
        // The Projects tab reads its all-time column from here instead of
        // re-summing the snapshot's entries, so the two must agree exactly.
        let (svc, _) = service("all-totals");
        let p = project(&svc);
        let q = svc.add_project("Other").unwrap();
        let now = now_ms();
        svc.add(&p.id, "one hour", now - 3_600_000, now).unwrap();
        svc.add(&p.id, "half hour", now - 1_800_000, now).unwrap();
        svc.add(&q.id, "quarter hour", now - 900_000, now).unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.all.total_ms, snap.total_ms);
        assert_eq!(snap.all.total_ms, 3_600_000 + 1_800_000 + 900_000);
        assert_eq!(snap.all.entry_count, snap.entries.len());
        assert_eq!(snap.all.per_project.get(&p.id), Some(&5_400_000));
        assert_eq!(snap.all.per_project.get(&q.id), Some(&900_000));
        // And the per-project figures match the entries themselves, so a
        // client summing the full entry list would arrive at the same row.
        for (id, ms) in &snap.all.per_project {
            let sum: i64 = snap
                .entries
                .iter()
                .filter(|e| &e.project_id == id)
                .map(|e| e.duration_ms())
                .sum();
            assert_eq!(&sum, ms, "project {id} disagrees with its entries");
        }
    }

    #[test]
    fn a_day_over_24h_is_reported_without_being_rejected() {
        let (svc, _) = service("over24");
        let p = project(&svc);
        let snap0 = svc.snapshot();
        let tz = core::Tz::from_snapshot(&snap0.tz, snap0.local_offset_ms);
        let today = tz.day_of(now_ms());
        let base = tz.day_start(today);
        // Three overlapping 12h entries, all starting today.
        svc.add(&p.id, "a", base, base + 12 * 3_600_000).unwrap();
        svc.add(&p.id, "b", base + 6 * 3_600_000, base + 18 * 3_600_000)
            .unwrap();
        svc.add(&p.id, "c", base + 12 * 3_600_000, base + 24 * 3_600_000)
            .unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.week.total_ms, 36 * 3_600_000, "the data is kept");
        assert!(snap.over_24h_days.contains(&today), "and flagged: {snap:?}");
    }

    #[test]
    fn archived_projects_keep_their_entries_in_totals() {
        let (svc, _) = service("archive");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "before archiving", now - 3_600_000, now)
            .unwrap();
        svc.set_archived(&p.id, true).unwrap();
        let snap = svc.snapshot();
        assert!(snap.projects[0].archived);
        assert_eq!(snap.week.total_ms, 3_600_000, "history is not rewritten");
    }

    #[test]
    fn a_fresh_store_has_no_entries_but_one_project() {
        // The seeded project is deliberate: with none, the Home tab had
        // nothing to attribute time to and its quick-add buttons were inert on
        // a fresh install.
        let (svc, _) = service("fresh");
        let snap = svc.snapshot();
        assert!(snap.entries.is_empty());
        assert_eq!(snap.total_ms, 0);
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.projects[0].name, "General");
        assert!(!snap.projects[0].archived, "the seed must be selectable");
    }

    #[test]
    fn the_seeded_project_is_persisted() {
        // Otherwise a restart would re-seed it and a rename would be lost.
        let (_svc, path) = service("seed-persist");
        let reopened = EntryService::new(JsonStore::new(path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.projects.len(), 1, "no duplicate seed after restart");
    }

    #[test]
    fn an_existing_store_is_never_seeded_over() {
        // Archiving the only project and restarting must not resurrect it as a
        // fresh "General", which would rewrite the user's history.
        let (svc, path) = service("no-reseed");
        let p = svc.snapshot().projects[0].id.clone();
        svc.set_archived(&p, true).unwrap();
        let reopened = EntryService::new(JsonStore::new(path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.projects.len(), 1);
        assert!(snap.projects[0].archived, "must stay archived");
    }

    #[test]
    fn time_can_be_added_to_a_fresh_store_without_creating_a_project_first() {
        // The regression this whole seed exists for.
        let (svc, _) = service("fresh-usable");
        let p = svc.snapshot().projects[0].id.clone();
        let e = svc.add_duration(&p, "", 15 * 60_000).unwrap();
        assert_eq!(e.duration_ms(), 15 * 60_000);
        assert_eq!(svc.snapshot().week.total_ms, 15 * 60_000);
    }

    // --- projects ---

    #[test]
    fn duplicate_project_names_are_refused() {
        let (svc, _) = service("dup");
        svc.add_project("Work").unwrap();
        assert!(svc.add_project("Work").is_err());
    }

    #[test]
    fn the_last_project_cannot_be_deleted() {
        // A fresh store is seeded with "General", so archiving it leaves
        // exactly one project and that one is undeletable.
        let (svc, _) = service("last-project");
        let seed = svc.snapshot().projects[0].id.clone();
        svc.set_archived(&seed, true).unwrap();
        assert_eq!(svc.snapshot().projects.len(), 1);
        assert!(svc.delete_project(&seed).is_err(), "last project");
    }

    #[test]
    fn projects_rename_and_recolour_through_the_service() {
        let (svc, _) = service("update-project");
        let id = svc.snapshot().projects[0].id.clone();
        let renamed = svc.update_project(&id, "Deep Work", -1).unwrap();
        assert_eq!(renamed.name, "Deep Work");
        assert_eq!(renamed.colour, None, "negative keeps the colour");
        let recoloured = svc.update_project(&id, "", 0x2ea043).unwrap();
        assert_eq!(recoloured.name, "Deep Work", "empty keeps the name");
        assert_eq!(recoloured.colour, Some(0x2ea043));
        // Entries stay attributed across the rename.
        let snap = svc.snapshot();
        assert_eq!(snap.projects[0].name, "Deep Work");
    }

    #[test]
    fn project_update_guards_are_typed() {
        use timetrack_core::RuleError;
        let (svc, _) = service("update-guards");
        svc.add_project("Other").unwrap();
        let id = svc.snapshot().projects[0].id.clone();
        assert_eq!(
            svc.update_project("nope", "X", -1).unwrap_err(),
            ServiceError::Rule(RuleError::UnknownProject("nope".into()))
        );
        assert_eq!(
            svc.update_project(&id, "   ", -1).unwrap_err(),
            ServiceError::Rule(RuleError::BlankProjectName)
        );
        assert_eq!(
            svc.update_project(&id, "Other", -1).unwrap_err(),
            ServiceError::Rule(RuleError::DuplicateProject("Other".into()))
        );
    }

    // --- timezone ---

    #[test]
    fn utc_parses_to_zero() {
        assert_eq!(core::Tz::parse("UTC"), Some(core::Tz::utc()));
        assert_eq!(core::Tz::parse("utc"), Some(core::Tz::utc()));
    }

    #[test]
    fn fixed_offsets_parse() {
        assert_eq!(
            core::Tz::parse("UTC+2").map(|t| t.offset_at_ms(0)),
            Some(7_200_000)
        );
        assert_eq!(
            core::Tz::parse("UTC+05:30").map(|t| t.offset_at_ms(0)),
            Some(19_800_000)
        );
        assert_eq!(
            core::Tz::parse("UTC-5").map(|t| t.offset_at_ms(0)),
            Some(-18_000_000)
        );
        assert_eq!(
            core::Tz::parse("UTC-08:00").map(|t| t.offset_at_ms(0)),
            Some(-28_800_000)
        );
    }

    #[test]
    fn a_named_zone_parses_via_the_tz_database() {
        // Previously this fell back to UTC and mis-bucketed near DST/week
        // boundaries; now it resolves per instant.
        let tz = core::Tz::parse("Europe/Rome").expect("Europe/Rome must parse");
        assert_eq!(tz.offset_at_ms(1_768_478_400_000), 3_600_000); // winter +1
        assert_eq!(tz.offset_at_ms(1_784_116_800_000), 7_200_000); // summer +2
    }

    #[test]
    fn out_of_range_offsets_are_refused() {
        assert_eq!(core::Tz::parse("UTC+25"), None);
        assert_eq!(core::Tz::parse("UTC+2:99"), None);
    }

    #[test]
    fn snapshot_carries_a_parseable_zone() {
        let (svc, _) = service("tz-name");
        let snap = svc.snapshot();
        let tz = core::Tz::from_snapshot(&snap.tz, snap.local_offset_ms);
        assert_eq!(tz.name(), snap.tz.as_str());
        assert_eq!(tz.offset_at_ms(now_ms()), snap.local_offset_ms);
    }

    #[test]
    fn dst_boundary_buckets_correctly_for_a_named_zone() {
        // 2026-07-05 22:30 UTC is Sunday in UTC but Monday 00:30 in Rome
        // (CEST): the service pinned to Europe/Rome must bucket and render
        // it as Monday, not as the Sunday a UTC fallback would choose.
        let dir = std::env::temp_dir().join("tt-svc-dst-rome");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let rome = core::Tz::parse("Europe/Rome").unwrap();
        let svc = EntryService::new_with_tz(JsonStore::new(path), rome.clone()).unwrap();
        assert_eq!(svc.snapshot().tz, "Europe/Rome");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        let utc_ms = 1_783_290_600_000; // 2026-07-05 22:30 UTC
        svc.add(&p.id, "late", utc_ms, utc_ms + 60_000).unwrap();

        let g = svc.inner.lock().expect("service state mutex poisoned");
        let rome_day = rome.day_of(utc_ms);
        let utc_day = core::Tz::utc().day_of(utc_ms);
        assert_ne!(rome_day, utc_day, "Sunday UTC vs Monday Rome");
        assert_eq!(
            core::week_totals(&g.data, rome_day, &rome).entry_count,
            1,
            "in Rome the entry is in the Monday week"
        );
        assert_eq!(
            core::week_totals(&g.data, utc_day, &rome).entry_count,
            0,
            "and not in the Sunday week"
        );
        drop(g);

        // Export renders the Rome wall-clock, not the UTC one.
        let csv = svc.export_csv("all").unwrap();
        assert!(
            csv.contains("2026-07-06T00:30:00"),
            "Rome local time, got:\n{csv}"
        );
    }

    #[test]
    fn now_is_after_2020() {
        // A sanity check on the one place the service reads the clock.
        assert!(now_ms() > 1_577_836_800_000, "clock looks wrong");
    }

    // --- CSV export ---

    #[test]
    fn export_returns_a_header_and_one_row_per_entry() {
        let (svc, _) = service("export");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "review", now - 3_600_000, now).unwrap();
        let csv = svc.export_csv("all").unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], timetrack_core::CSV_HEADER);
        assert!(lines[1].contains("review"), "row: {}", lines[1]);
        assert!(lines[1].contains("01:00:00"), "row: {}", lines[1]);
    }

    #[test]
    fn export_rejects_an_unknown_scope() {
        let (svc, _) = service("export-scope");
        assert!(svc.export_csv("everything").is_err());
    }

    // --- description editing ---

    #[test]
    fn description_can_be_rewritten() {
        let (svc, _) = service("settext");
        let p = project(&svc);
        let now = now_ms();
        let e = svc.add(&p.id, "typo", now - 3_600_000, now).unwrap();
        let edited = svc.set_text(&e.id, "review").unwrap();
        assert_eq!(edited.description, "review");
        assert_eq!(svc.snapshot().entries[0].description, "review");
    }

    #[test]
    fn editing_an_unknown_entry_is_an_error() {
        let (svc, _) = service("settext-unknown");
        assert!(svc.set_text("nope", "x").is_err());
    }

    // --- project reassignment ---

    #[test]
    fn an_entry_moves_to_another_project() {
        let (svc, _) = service("setproject");
        let p = project(&svc);
        let q = svc.add_project("Other").unwrap();
        let now = now_ms();
        let e = svc.add(&p.id, "movable", now - 3_600_000, now).unwrap();
        let moved = svc.set_project(&e.id, &q.id).unwrap();
        assert_eq!(moved.project_id, q.id);
        let snap = svc.snapshot();
        assert_eq!(
            snap.week.per_project.get(&q.id),
            Some(&3_600_000),
            "the time travels with the entry"
        );
        assert!(snap.week.per_project.get(&p.id).is_none());
    }

    #[test]
    fn reassignment_names_unknown_ids() {
        use timetrack_core::RuleError;
        let (svc, _) = service("setproject-guards");
        let p = project(&svc);
        let now = now_ms();
        let e = svc.add(&p.id, "stays", now - 3_600_000, now).unwrap();
        assert_eq!(
            svc.set_project("nope", &p.id).unwrap_err(),
            ServiceError::Rule(RuleError::UnknownEntry("nope".into()))
        );
        assert_eq!(
            svc.set_project(&e.id, "nope").unwrap_err(),
            ServiceError::Rule(RuleError::UnknownProject("nope".into()))
        );
        assert_eq!(svc.snapshot().entries.len(), 1, "refusals move nothing");
    }
}
