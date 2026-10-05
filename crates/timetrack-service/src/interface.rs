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
use timetrack_proto::{self as protocol, EntryView, ProjectView, Snapshot, TotalsView};

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
    /// The zone's offset from UTC, resolved once at startup.
    local_offset_ms: i64,
}

/// Serialise access to the store.
pub struct EntryService {
    inner: Mutex<Inner>,
}

impl EntryService {
    pub fn new(store: JsonStore) -> anyhow::Result<Self> {
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
        let local_offset_ms = resolve_offset();
        let service = EntryService {
            inner: Mutex::new(Inner {
                store,
                data,
                ids,
                local_offset_ms,
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
        let today = core::local_day_of(now, g.local_offset_ms);
        let week = core::week_totals(&g.data, today, g.local_offset_ms);
        let month = core::month_totals(&g.data, today, g.local_offset_ms);

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
            total_ms: g.data.total_ms(),
            over_24h_days: core::days_exceeding_24h(&g.data, g.local_offset_ms),
            local_offset_ms: g.local_offset_ms,
        }
    }

    /// Apply a mutation to the store, persist, and return the result.
    ///
    /// The single funnel every mutating method goes through, which is what
    /// guarantees the persist-before-acknowledge ordering. If the write fails
    /// the in-memory copy is reloaded from disk, so the service never reports
    /// success on data it could not store.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Store, &mut Counter, i64) -> core::RuleResult<T>,
    ) -> anyhow::Result<T> {
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
                // Persist before acknowledging. If the write fails the
                // in-memory copy is stale, so reload rather than report
                // success on data that is not on disk.
                if let Err(e) = store.save(data) {
                    *data = store.load().unwrap_or_default();
                    return Err(anyhow::Error::from(e));
                }
                Ok(value)
            }
            Err(rule) => Err(anyhow::Error::msg(rule.to_string())),
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
    ) -> anyhow::Result<EntryView> {
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
    ) -> anyhow::Result<EntryView> {
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
    ) -> anyhow::Result<EntryView> {
        self.add(project_id, description, ended_at - duration_ms, ended_at)
    }

    /// Method 4: one of the quick-add buckets, ending now.
    pub fn quick_add(&self, project_id: &str, duration_ms: i64) -> anyhow::Result<EntryView> {
        let e = self.mutate(|s, ids, now| core::quick_add(s, ids, project_id, duration_ms, now))?;
        Ok(protocol::entry_to_view(&e))
    }

    // --- editing ---

    /// Shorten by moving an endpoint. Never deletes (REQUIREMENTS §5).
    pub fn set_times(&self, id: &str, started_at: i64, ended_at: i64) -> anyhow::Result<EntryView> {
        let e = self.mutate(|s, _ids, _| core::set_times(s, id, started_at, ended_at))?;
        Ok(protocol::entry_to_view(&e))
    }

    /// Undo exactly the entry a quick-add created.
    ///
    /// Refuses anything not created by quick-add, so a client's "undo" can
    /// never delete hand-entered time by mistake.
    pub fn undo_quick_add(&self, id: &str) -> anyhow::Result<()> {
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
    pub fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        self.mutate(|s, _ids, _| core::delete_entry(s, id).map(|_| ()))
    }

    pub fn split(&self, id: &str, at_ms: i64) -> anyhow::Result<(EntryView, EntryView)> {
        let (a, b) = self.mutate(|s, ids, _| core::split_entry(s, ids, id, at_ms))?;
        Ok((protocol::entry_to_view(&a), protocol::entry_to_view(&b)))
    }

    /// Merge entries into one spanning the union of their intervals.
    pub fn merge(&self, ids: &[String]) -> anyhow::Result<EntryView> {
        let e = self.mutate(|s, _ids, _| core::merge_entries(s, ids))?;
        Ok(protocol::entry_to_view(&e))
    }

    // --- projects ---

    pub fn add_project(&self, name: &str) -> anyhow::Result<ProjectView> {
        let p = self.mutate(|s, ids, _| core::create_project(s, ids, name))?;
        Ok(protocol::project_to_view(&p))
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> anyhow::Result<ProjectView> {
        let p = self.mutate(|s, _ids, _| core::set_archived(s, id, archived))?;
        Ok(protocol::project_to_view(&p))
    }

    pub fn delete_project(&self, id: &str) -> anyhow::Result<()> {
        self.mutate(|s, _ids, _| core::delete_project(s, id).map(|_| ()))
    }

    /// CSV export (REQUIREMENTS §10): one row per entry in `scope`, with a
    /// header, ISO 8601 local timestamps, `HH:MM:SS` + minutes, and `source`.
    pub fn export_csv(&self, scope: &str) -> anyhow::Result<String> {
        let scope = core::ExportScope::parse(scope).ok_or_else(|| {
            anyhow::anyhow!("unknown export scope '{scope}' (use week, month or all)")
        })?;
        let g = self.inner.lock().expect("service state mutex poisoned");
        let now = now_ms();
        Ok(core::export_csv(&g.data, scope, now, g.local_offset_ms))
    }
}

fn to_totals_view(t: &core::Totals) -> TotalsView {
    TotalsView {
        total_ms: t.total_ms,
        per_project: t.per_project.clone(),
        entry_count: t.entry_count,
    }
}

/// Resolve the local UTC offset, in milliseconds.
///
/// # Known limitation
///
/// This resolves the offset *once*, at startup, and only for fixed-offset
/// zones. A machine that crosses a DST boundary while the service is running
/// keeps the offset it resolved at launch, so an entry made after the change
/// can be bucketed into the wrong local day. The real fix is a tz-database
/// lookup per instant; `PLAN.md` carries that as its own increment. Until then
/// this is exact for every zone without DST, and can be off by an hour within
/// a few hours either side of a transition.
fn resolve_offset() -> i64 {
    match std::env::var("TZ") {
        Ok(tz) => parse_fixed_tz(&tz).unwrap_or(0),
        Err(_) => 0,
    }
}

/// Parse the unambiguous subset of `TZ`: `UTC+2`, `UTC-05:30` and friends.
///
/// Only fixed-offset zones are handled. A named zone needs the tz database, and
/// mis-parsing one would put entries in the wrong local day, which is worse
/// than falling back to UTC.
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
        // The mutate funnel reloads from disk on a save error, so a client
        // never sees an entry the service could not persist.
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
    fn a_day_over_24h_is_reported_without_being_rejected() {
        let (svc, _) = service("over24");
        let p = project(&svc);
        let today = core::local_day_of(now_ms(), svc.snapshot().local_offset_ms);
        let base = core::local_day_start(today, svc.snapshot().local_offset_ms);
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

    // --- timezone parsing ---

    #[test]
    fn utc_is_zero() {
        assert_eq!(parse_fixed_tz("UTC"), Some(0));
        assert_eq!(parse_fixed_tz("utc"), Some(0));
    }

    #[test]
    fn fixed_offsets_parse() {
        assert_eq!(parse_fixed_tz("UTC+2"), Some(7_200_000));
        assert_eq!(parse_fixed_tz("UTC+05:30"), Some(19_800_000));
        assert_eq!(parse_fixed_tz("UTC-5"), Some(-18_000_000));
        assert_eq!(parse_fixed_tz("UTC-08:00"), Some(-28_800_000));
    }

    #[test]
    fn a_named_zone_is_refused_rather_than_mis_parsed() {
        // Parsing "Europe/Rome" as an offset would put entries in the wrong
        // local day, so it must be refused.
        assert_eq!(parse_fixed_tz("Europe/Rome"), None);
        assert_eq!(parse_fixed_tz("CET"), None);
    }

    #[test]
    fn out_of_range_offsets_are_refused() {
        assert_eq!(parse_fixed_tz("UTC+25"), None);
        assert_eq!(parse_fixed_tz("UTC+2:99"), None);
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
}
