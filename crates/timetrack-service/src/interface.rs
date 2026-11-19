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
//! "added" must be able to rely on the entry surviving a crash. Since §17
//! the persisted form is one SQLite transaction holding the mutation's diff
//! (`timetrack-store`), not a rewrite of the whole store.
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

use std::sync::{Mutex, MutexGuard};

/// Take the state lock, recovering from poison (same pattern as the tray,
/// #100).
///
/// A poisoned lock still holds its data: the panic that poisoned it happened
/// elsewhere, and the state behind it is intact. `expect`-ing the poison
/// turned one panic into every later call failing, so recover and go on.
fn lock(mutex: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

use timetrack_core as core;
use timetrack_core::{EntrySource, RuleError, Store};
use timetrack_proto::{
    self as protocol, EntryView, ProjectView, ServiceError, Snapshot, TotalsView,
};
use timetrack_store::SqliteStore;

use crate::credentials::{CredentialStore, OsCredentialStore};
use std::sync::Arc;

/// UUID identities, minted here at the service boundary (ADR 0018).
///
/// The core stays randomness-free behind its [`core::Ids`] trait: this
/// is the production generator, while [`core::Counter`] keeps serving
/// as the deterministic test fake. Stateless — every mint is a fresh
/// UUIDv4, so unlike the old counter there is nothing to resume after
/// a restart and nothing two stores could collide on.
// Clone keeps the mutation funnel's working-copy pattern (#100): the
// clone is discarded when the persist fails.
#[derive(Debug, Clone, Default)]
struct UuidIds;

impl core::Ids for UuidIds {
    fn next(&mut self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn next_project(&mut self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

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
    store: SqliteStore,
    data: Store,
    ids: UuidIds,
    /// Per-source secrets (#149): the OS store in production, the fake
    /// in tests. Behind an `Arc` so fetch paths can hold it without the
    /// state lock.
    credentials: Arc<dyn CredentialStore>,
    /// The local zone (#102): resolved strictly at startup from `TZ` (or
    /// the system zone), then re-resolved before every snapshot, mutation
    /// and export so a system timezone change (travel) is picked up live
    /// instead of needing a restart. Offsets are resolved per instant
    /// from this, so DST transitions bucket correctly (REQUIREMENTS §9).
    tz: core::Tz,
    /// Whether `tz` may be refreshed from the environment. Production
    /// builds it live; `new_with_tz` pins it so tests stay deterministic.
    tz_live: bool,
    /// The last refresh failure already logged, so a zone that becomes
    /// unusable mid-run is noted once instead of once per snapshot poll.
    /// The last good zone is kept in that case: startup already proved a
    /// usable zone exists, and failing every call would lose tracking.
    tz_refresh_error: Option<String>,
}

/// Re-resolve the zone from the environment (#102), adopting it when it
/// changed. `TZ` in the process environment cannot change after launch —
/// that genuinely needs a restart — but the *system* zone can (the user
/// travels, `timedatectl` runs), and that is the case this picks up.
fn refresh_tz(inner: &mut Inner) {
    if !inner.tz_live {
        return;
    }
    match core::Tz::try_from_env() {
        Ok(fresh) => {
            inner.tz_refresh_error = None;
            if fresh.name() != inner.tz.name() {
                inner.tz = fresh;
            }
        }
        Err(e) => {
            let note = e.to_string();
            if inner.tz_refresh_error.as_deref() != Some(note.as_str()) {
                eprintln!(
                    "timetrack-service: keeping timezone {} ({note})",
                    inner.tz.name()
                );
                inner.tz_refresh_error = Some(note);
            }
        }
    }
}

/// Serialise access to the store.
pub struct EntryService {
    inner: Mutex<Inner>,
    /// `Changed` subscribers on the socket transport (§16). The D-Bus
    /// signal is emitted by the interface wrappers in `main.rs`; socket
    /// subscribers are notified here, in the mutation funnel, so a
    /// mutation arriving over *either* transport wakes subscribers on
    /// both. Dead receivers are dropped on the next notify.
    changed: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl EntryService {
    pub fn new(store: SqliteStore) -> anyhow::Result<Self> {
        // Strict (#102): an unusable `TZ` or system zone stops the service
        // here with an actionable message, instead of silently running in
        // UTC and bucketing every typed local time into the wrong day.
        let tz = core::Tz::try_from_env().map_err(anyhow::Error::new)?;
        Self::new_inner(store, tz, true, Arc::new(OsCredentialStore::new()))
    }

    /// Build with an explicit, pinned zone. The production path is
    /// [`EntryService::new`] (from the environment, refreshed live); this
    /// exists so tests can pin a named zone without mutating the
    /// process-global `TZ`, and pinning turns the live refresh off so the
    /// test's zone can never be replaced under it.
    #[cfg(test)]
    pub fn new_with_tz(store: SqliteStore, tz: core::Tz) -> anyhow::Result<Self> {
        Self::new_inner(store, tz, false, Arc::new(OsCredentialStore::new()))
    }

    /// Build with an injected credential backend (#149): the fake, so
    /// credential tests never touch a real OS store.
    #[cfg(test)]
    pub fn new_with_credentials(
        store: SqliteStore,
        credentials: Arc<dyn CredentialStore>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(store, core::Tz::utc(), false, credentials)
    }

    fn new_inner(
        store: SqliteStore,
        tz: core::Tz,
        tz_live: bool,
        credentials: Arc<dyn CredentialStore>,
    ) -> anyhow::Result<Self> {
        let mut data = store.load()?;
        // A brand-new store has no projects, and with no project there is
        // nothing to attribute time to -- which made the Home tab's quick-add
        // buttons do nothing at all on a fresh install. Seed one so the app
        // is usable the moment it opens. Only on a genuinely empty store: an
        // existing one is left exactly as it is, archived projects and all.
        let seeded = data.projects.is_empty();
        if seeded {
            data.projects.push(timetrack_core::Project {
                id: uuid::Uuid::new_v4().to_string(),
                name: "General".into(),
                colour: None,
                archived: false,
            });
        }
        let ids = UuidIds;
        let service = EntryService {
            inner: Mutex::new(Inner {
                store,
                data,
                ids,
                credentials,
                tz,
                tz_live,
                tz_refresh_error: None,
            }),
            changed: Mutex::new(Vec::new()),
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

    /// Subscribe to persisted mutations (the socket transport's
    /// `Changed`, §16). Each successful mutation sends one `()` here;
    /// coalescing by the reader is fine, as on the bus.
    pub fn subscribe_changed(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    fn notify_changed(&self) {
        self.changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|tx| tx.send(()).is_ok());
    }

    /// Write the current state out. Used by the mutation funnel and by
    /// startup, so both go through the same path.
    fn persist(&self) -> anyhow::Result<()> {
        let g = lock(&self.inner);
        g.store
            .save_all(&g.data, now_ms())
            .map_err(anyhow::Error::from)
    }

    /// A consistent view of everything a client needs for one frame.
    pub fn snapshot(&self) -> Snapshot {
        let mut g = lock(&self.inner);
        refresh_tz(&mut g);
        let now = now_ms();
        let today = g.tz.day_of(now);
        // One pass for all four aggregates: clients poll snapshots
        // continuously, so four full rescans per poll made every client's
        // idle refresh cost grow with the whole history, under the lock
        // mutations wait on.
        let totals = core::snapshot_totals(&g.data, today, &g.tz);

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
            week: to_totals_view(&totals.week),
            month: to_totals_view(&totals.month),
            all: to_totals_view(&totals.all),
            total_ms: totals.all.total_ms,
            over_24h_days: totals.over_24h_days,
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
    /// If the write fails the mutation is rolled back, so memory always
    /// matches disk (#100): a mutation the client was told failed must not
    /// linger in snapshots it was never acknowledged for.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Store, &mut dyn core::Ids, i64) -> core::RuleResult<T>,
    ) -> Result<T, ServiceError> {
        let mut g = lock(&self.inner);
        refresh_tz(&mut g);
        let now = now_ms();
        // Destructure so `data` and `ids` are borrowed separately; borrowing
        // two fields of the same struct mutably in one call is rejected.
        let Inner {
            data, ids, store, ..
        } = &mut *g;
        // Mutate a working copy and swap it in only once it is on disk, so
        // a failed persist (or a rule refusal after a partial mutation)
        // leaves memory holding exactly what the database holds (#100).
        // The copy is the one full-store clone a mutation pays for:
        // mutating in place needed the same clone kept aside for the
        // rollback, and the working copy doubles as the "after" side of
        // the diff the store applies.
        let mut next_data = data.clone();
        let mut next_ids = ids.clone();
        match f(&mut next_data, &mut next_ids, now) {
            Ok(value) => {
                // Persist before acknowledging -- and before the new
                // state becomes visible to snapshots. The persisted form
                // is one SQLite transaction holding this mutation's diff
                // (§17), so a tap stays proportional to the change.
                let record = core::Journal::between(data, &next_data);
                if let Err(e) = store.apply(&record, now) {
                    return Err(ServiceError::Storage(e.to_string()));
                }
                *data = next_data;
                *ids = next_ids;
                drop(g);
                self.notify_changed();
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
    ///
    /// The start is computed in the core, checked (#98): this method used to
    /// subtract here, unchecked, so an extreme wire pair wrapped (release)
    /// or panicked (debug) before any rule saw it.
    pub fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, ids, _| {
            core::create_duration_ending(s, ids, project_id, description, duration_ms, ended_at)
        })?;
        Ok(protocol::entry_to_view(&e))
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
            // The one lookup this layer does itself rather than through
            // a rule: resolve the same way, so prefixes work here too.
            let id = core::resolve_entry_id(s, id)?;
            let entry = s.get(&id).expect("resolved above");
            if entry.source != EntrySource::QuickAdd {
                return Err(RuleError::NotQuickAdd(id.clone()));
            }
            core::delete_entry(s, &id).map(|_| ())
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
    /// An empty `name` keeps the current name; `colour` -1 keeps the current
    /// colour, -2 clears it, otherwise the low 24 bits become the new
    /// `0xRRGGBB`. (D-Bus has no `Option` on the wire, so "leave it alone"
    /// and "clear it" need these sentinels; the core function underneath
    /// still takes real options.) Clearing arrived with the GUI recolour
    /// prompt (#104): an old service reads -2 as "keep", so a new client
    /// degrades to a no-op there rather than corrupting anything; other
    /// negatives keep, as every negative used to.
    pub fn update_project(
        &self,
        id: &str,
        name: &str,
        colour: i64,
    ) -> Result<ProjectView, ServiceError> {
        let name = if name.is_empty() { None } else { Some(name) };
        let colour = match colour {
            -2 => Some(None),
            c if c < 0 => None,
            c => Some(Some((c as u32) & 0x00FF_FFFF)),
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

    /// CSV export anchored at `anchor_ms` (REQUIREMENTS §10): one row per
    /// entry in `scope` for the week or month containing the anchor, with a
    /// header, ISO 8601 local timestamps, `HH:MM:SS` + minutes, and `source`.
    /// All time ignores the anchor entirely. The anchor is already an instant
    /// (the client parsed the day to midday in the snapshot zone); the
    /// service buckets it with its own zone, so no new timezone logic lives
    /// here.
    pub fn export_csv_at(&self, scope: &str, anchor_ms: i64) -> Result<String, ServiceError> {
        let scope = core::ExportScope::parse(scope)
            .ok_or_else(|| ServiceError::UnknownExportScope(scope.to_string()))?;
        // The service edge of the supported range (#97): a week/month anchor
        // outside years 0001-9999 is refused typed instead of bucketing at a
        // clamped edge the caller never asked about. All time ignores the
        // anchor entirely, so it stays unexamined there.
        if scope != core::ExportScope::All && !core::instant_in_range(anchor_ms) {
            return Err(ServiceError::from(RuleError::InstantOutOfRange(anchor_ms)));
        }
        let mut g = lock(&self.inner);
        refresh_tz(&mut g);
        Ok(core::export_csv_at(&g.data, scope, anchor_ms, &g.tz))
    }

    /// CSV export (REQUIREMENTS §10): one row per entry in `scope`, with a
    /// header, ISO 8601 local timestamps, `HH:MM:SS` + minutes, and `source`.
    ///
    /// Backward-compat wrapper over [`EntryService::export_csv_at`],
    /// anchored at now: old clients keep current-period behaviour unchanged
    /// against a new service.
    pub fn export_csv(&self, scope: &str) -> Result<String, ServiceError> {
        self.export_csv_at(scope, now_ms())
    }

    /// CSV export between two local days, inclusive (#133). The days
    /// are bucketed here with the service zone, as every scoped export.
    pub fn export_csv_range(&self, from_ms: i64, to_ms: i64) -> Result<String, ServiceError> {
        for instant in [from_ms, to_ms] {
            if !core::instant_in_range(instant) {
                return Err(ServiceError::from(RuleError::InstantOutOfRange(instant)));
            }
        }
        let mut g = lock(&self.inner);
        refresh_tz(&mut g);
        if g.tz.day_of(from_ms) > g.tz.day_of(to_ms) {
            return Err(ServiceError::Other(
                "the range ends before it starts".into(),
            ));
        }
        Ok(core::export_csv_between(&g.data, from_ms, to_ms, &g.tz))
    }

    // --- external sources (§19) ---
    //
    // Fetched read-only in the service, cached as suggestions in the
    // store, and turned into entries only on confirm. Nothing here
    // writes back to a source, ever.

    pub fn add_source(
        &self,
        kind: &str,
        locator: &str,
        project_id: Option<&str>,
        label: &str,
    ) -> Result<protocol::SourceView, ServiceError> {
        validate_source_identity(kind, locator)?;
        let g = lock(&self.inner);
        // Resolve before registering: a prefix names its project, and an
        // ambiguous one refuses before any row is written.
        let project_id = project_id
            .map(|pid| core::resolve_project_id(&g.data, pid))
            .transpose()?;
        let label = if label.trim().is_empty() {
            locator.rsplit('/').next().unwrap_or(locator)
        } else {
            label
        };
        let already = g
            .store
            .sources()
            .map_err(|e| ServiceError::Storage(e.to_string()))?
            .iter()
            .any(|s| s.kind == kind && s.locator == locator);
        let src = g
            .store
            .add_source(kind, locator, project_id.as_deref(), label, now_ms())
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        drop(g);
        if !already {
            self.notify_changed();
        }
        Ok(source_view(src))
    }

    /// Rewrite a source (#153): kind, locator, project mapping, and label
    /// are all replaced with the given state. The same validation as
    /// registration applies (bad kind, empty locator, a CalDAV password
    /// in the URL), plus the edit's own: the id must exist, and the new
    /// `(kind, locator)` must not belong to another source. An empty
    /// label re-derives from the locator, as in registration.
    pub fn update_source(
        &self,
        id: &str,
        kind: &str,
        locator: &str,
        project_id: Option<&str>,
        label: &str,
    ) -> Result<protocol::SourceView, ServiceError> {
        validate_source_identity(kind, locator)?;
        let g = lock(&self.inner);
        let current = resolve_source(&g.store, id)?;
        // Resolve before writing, as in registration.
        let project_id = project_id
            .map(|pid| core::resolve_project_id(&g.data, pid))
            .transpose()?;
        if g.store
            .sources()
            .map_err(|e| ServiceError::Storage(e.to_string()))?
            .iter()
            .any(|s| s.id != current.id && s.kind == kind && s.locator == locator)
        {
            return Err(ServiceError::Other(format!(
                "a {kind} source for '{locator}' already exists"
            )));
        }
        let label = if label.trim().is_empty() {
            locator.rsplit('/').next().unwrap_or(locator)
        } else {
            label
        };
        let src = g
            .store
            .update_source(
                &current.id,
                kind,
                locator,
                project_id.as_deref(),
                label,
                now_ms(),
            )
            .map_err(|e| ServiceError::Storage(e.to_string()))?
            .ok_or_else(|| ServiceError::Rule(RuleError::UnknownSource(id.to_string())))?;
        drop(g);
        self.notify_changed();
        Ok(source_view(src))
    }

    pub fn sources(&self) -> Result<Vec<protocol::SourceView>, ServiceError> {
        let g = lock(&self.inner);
        Ok(g.store
            .sources()
            .map_err(|e| ServiceError::Storage(e.to_string()))?
            .into_iter()
            .map(source_view)
            .collect())
    }

    /// Fetch one source (or all), parse, and store new suggestions.
    /// Fetching never creates entries and never writes back.
    pub fn refresh_source(&self, id: Option<&str>) -> Result<protocol::RefreshView, ServiceError> {
        let (tz, targets, credentials) = {
            let mut g = lock(&self.inner);
            refresh_tz(&mut g);
            let targets: Vec<_> = match id {
                Some(want) => vec![resolve_source(&g.store, want)?],
                None => g
                    .store
                    .sources()
                    .map_err(|e| ServiceError::Storage(e.to_string()))?,
            };
            (g.tz.clone(), targets, g.credentials.clone())
        };
        let mut total = protocol::RefreshView::default();
        // Suggestions are actionable near-term: expand recurrences a
        // month back (just-missed meetings still get logged) and a
        // quarter ahead, nothing further in either direction.
        let now = now_ms();
        let (from_ms, until_ms) = (now - 31 * 86_400_000, now + 93 * 86_400_000);
        for src in &targets {
            if src.kind == "caldav" {
                let tally = self.refresh_caldav_source(src, &tz, from_ms, until_ms)?;
                total.fetched += tally.fetched;
                total.inserted += tally.inserted;
                total.skipped += tally.skipped;
                total.updated += tally.updated;
                total.withdrawn += tally.withdrawn;
                continue;
            }
            for text in fetch_source_texts(src, &*credentials)? {
                let parsed =
                    core::parse_ics_in_window(&text, &tz, from_ms, until_ms).map_err(|_| {
                        ServiceError::Other(format!("{} is not an iCalendar file", src.locator))
                    })?;
                total.fetched += parsed.items.len();
                total.skipped += parsed.skipped;
                let g = lock(&self.inner);
                total.inserted += g
                    .store
                    .upsert_suggestions(&src.id, &parsed.items, now_ms())
                    .map_err(|e| ServiceError::Storage(e.to_string()))?;
            }
        }
        Ok(total)
    }

    /// Refresh one CalDAV source incrementally (#151): fetch only what
    /// changed since the stored sync state, reconcile each changed
    /// resource into pending suggestions, withdraw what the server
    /// deleted, and record the new state last — the token/CTag claims
    /// "seen it all", so it must never get ahead of the data.
    ///
    /// Every fetched body parses before anything is stored, so a
    /// corrupt resource fails the refresh with the previous state
    /// untouched instead of half-applying it.
    fn refresh_caldav_source(
        &self,
        src: &timetrack_store::ExternalSource,
        tz: &core::Tz,
        from_ms: i64,
        until_ms: i64,
    ) -> Result<protocol::RefreshView, ServiceError> {
        // The fetch matrix (#149): a stored credential authenticates,
        // and only a source without one reads the headless fallback.
        let credentials = lock(&self.inner).credentials.clone();
        let env_password = std::env::var("TIMETRACK_CALDAV_PASSWORD").ok();
        let auth = crate::credentials::resolve_caldav_auth(
            src.cred_ref.as_deref(),
            src.cred_username.as_deref(),
            &src.locator,
            &|reference| credentials.get(reference),
            env_password.as_deref(),
        )
        .map_err(ServiceError::Other)?;
        let (state, known) = {
            let g = lock(&self.inner);
            let state = g
                .store
                .caldav_sync_state(&src.id)
                .map_err(|e| ServiceError::Storage(e.to_string()))?
                .unwrap_or_default();
            let known = g
                .store
                .caldav_resources(&src.id)
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
            (state, known)
        };
        // Off the state lock: the fetch is network I/O.
        let outcome = crate::caldav::fetch_calendar_incremental(
            &src.locator,
            auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
            &crate::caldav::SyncState {
                sync_token: state.sync_token,
                ctag: state.ctag,
            },
            &known
                .iter()
                .map(|k| crate::caldav::StoredValidator {
                    href: k.href.clone(),
                    etag: k.etag.clone(),
                    content_hash: k.content_hash.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .map_err(ServiceError::Other)?;
        let mut parsed_all = Vec::with_capacity(outcome.fetched.len());
        for resource in &outcome.fetched {
            let parsed =
                core::parse_ics_in_window(&resource.body, tz, from_ms, until_ms).map_err(|_| {
                    ServiceError::Other(format!("{} is not an iCalendar file", resource.href))
                })?;
            parsed_all.push((resource, parsed));
        }
        let mut tally = protocol::RefreshView::default();
        let g = lock(&self.inner);
        for (resource, parsed) in &parsed_all {
            tally.fetched += parsed.items.len();
            tally.skipped += parsed.skipped;
            let counts = g
                .store
                .reconcile_caldav_resource(&src.id, &resource.href, &parsed.items, now_ms())
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
            tally.inserted += counts.inserted;
            tally.updated += counts.updated;
            tally.withdrawn += counts.withdrawn;
            g.store
                .upsert_caldav_resource(
                    &src.id,
                    &resource.href,
                    resource.etag.as_deref(),
                    Some(&resource.content_hash),
                    now_ms(),
                )
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
        }
        for validator in &outcome.refreshed {
            g.store
                .upsert_caldav_resource(
                    &src.id,
                    &validator.href,
                    validator.etag.as_deref(),
                    validator.content_hash.as_deref(),
                    now_ms(),
                )
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
        }
        for href in &outcome.removed_hrefs {
            tally.withdrawn += g
                .store
                .withdraw_caldav_resource(&src.id, href)
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
        }
        g.store
            .set_caldav_sync_state(
                &src.id,
                outcome.sync_token.as_deref(),
                outcome.ctag.as_deref(),
                now_ms(),
            )
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        Ok(tally)
    }

    pub fn suggestions(&self) -> Result<Vec<protocol::SuggestionView>, ServiceError> {
        let g = lock(&self.inner);
        Ok(g.store
            .suggestions(true)
            .map_err(|e| ServiceError::Storage(e.to_string()))?
            .into_iter()
            .map(suggestion_view)
            .collect())
    }

    /// Confirm a pending suggestion into a real `External` entry. The
    /// suggestion is claimed first (pending -> confirmed), so a racing
    /// second confirm refuses instead of double-creating.
    pub fn confirm_suggestion(
        &self,
        id: &str,
        project_id: Option<&str>,
    ) -> Result<EntryView, ServiceError> {
        let (suggestion, fallback_project) = {
            let g = lock(&self.inner);
            let suggestion = g
                .store
                .suggestion(id)
                .map_err(|e| ServiceError::Storage(e.to_string()))?
                .ok_or_else(|| ServiceError::Other(format!("no suggestion with id '{id}'")))?;
            if suggestion.status != "pending" {
                return Err(ServiceError::Other(format!(
                    "suggestion '{id}' is already {}",
                    suggestion.status
                )));
            }
            let source_project = g
                .store
                .source(&suggestion.source_id)
                .map_err(|e| ServiceError::Storage(e.to_string()))?
                .and_then(|s| s.project_id);
            // Resolve the override — or validate the mapped/first-active
            // fallback — before the suggestion is claimed, so a bad
            // project refuses with the suggestion still pending.
            let fallback = match project_id {
                Some(pid) => core::resolve_project_id(&g.data, pid)?,
                None => {
                    let candidate = source_project
                        .or_else(|| {
                            g.data
                                .projects
                                .iter()
                                .find(|p| !p.archived)
                                .map(|p| p.id.clone())
                        })
                        .ok_or_else(|| ServiceError::Other("no project to confirm into".into()))?;
                    // A stored mapping can dangle when its project was
                    // deleted after the source was registered: refuse
                    // before the claim, not after.
                    core::resolve_project_id(&g.data, &candidate)?
                }
            };
            (suggestion, fallback)
        };
        {
            let g = lock(&self.inner);
            let claimed = g
                .store
                .decide_suggestion(id, "confirmed", None, now_ms())
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
            if !claimed {
                return Err(ServiceError::Other(format!(
                    "suggestion '{id}' is already decided"
                )));
            }
        }
        let entry = self.mutate(|s, ids, _| {
            core::create_entry_from(
                s,
                ids,
                &fallback_project,
                &suggestion.title,
                suggestion.started_at,
                suggestion.ended_at,
                EntrySource::External,
            )
        })?;
        let view = protocol::entry_to_view(&entry);
        let g = lock(&self.inner);
        g.store
            .link_suggestion_entry(id, &entry.id)
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        Ok(view)
    }

    pub fn dismiss_suggestion(&self, id: &str) -> Result<(), ServiceError> {
        let g = lock(&self.inner);
        let decided = g
            .store
            .decide_suggestion(id, "dismissed", None, now_ms())
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        if decided {
            Ok(())
        } else {
            Err(ServiceError::Other(format!(
                "suggestion '{id}' is not pending"
            )))
        }
    }

    // --- source credentials (#149) ---
    //
    // The secret goes to the OS store; SQLite keeps only the reference
    // and the login name. The same operations the future GUI source
    // editor will call.

    /// Store (or replace) a source's secret in the OS credential store.
    /// An empty `username` falls back to the locator's userinfo, so a
    /// `https://user@host/…` source needs no `--username`; a locator
    /// without one refuses until a name is given.
    pub fn set_source_credential(
        &self,
        id: &str,
        username: &str,
        secret: &str,
    ) -> Result<protocol::SourceView, ServiceError> {
        if secret.is_empty() {
            return Err(ServiceError::Other(
                "the credential is empty; pass the secret on stdin".into(),
            ));
        }
        let (source, reference) = {
            let g = lock(&self.inner);
            let source = resolve_source(&g.store, id)?;
            let reference = crate::credentials::credential_ref(&source.id);
            (source, reference)
        };
        let username = if username.trim().is_empty() {
            let (_, locator_user) =
                crate::caldav::split_userinfo(&source.locator).map_err(ServiceError::Other)?;
            locator_user.ok_or_else(|| {
                ServiceError::Other(format!(
                    "source '{}' names no user; pass --username <name>",
                    source.id
                ))
            })?
        } else {
            username.to_string()
        };
        // The fallible external write first: when the OS store refuses,
        // the database is untouched and the source keeps working as
        // before (env fallback included).
        let credentials = lock(&self.inner).credentials.clone();
        credentials
            .set(&reference, secret)
            .map_err(|e| ServiceError::Other(e.to_string()))?;
        let recorded = lock(&self.inner)
            .store
            .set_source_credential_ref(&source.id, &reference, &username)
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        if !recorded {
            // The source vanished between the lookup and the write (a
            // racing remove): roll the orphaned OS entry back so no
            // unreachable secret is left behind.
            let _ = credentials.delete(&reference);
            return Err(ServiceError::Rule(RuleError::UnknownSource(
                source.id.clone(),
            )));
        }
        if let Some(old_ref) = source.cred_ref.as_deref()
            && old_ref != reference
        {
            let _ = credentials.delete(old_ref);
        }
        let view = self
            .sources()?
            .into_iter()
            .find(|s| s.id == source.id)
            .ok_or_else(|| ServiceError::Rule(RuleError::UnknownSource(source.id.clone())))?;
        self.notify_changed();
        Ok(view)
    }

    /// Forget a source's credential: the OS entry is deleted first, and
    /// the reference is cleared only after, so a failed delete leaves
    /// the source exactly as it was. Removing what was never set is a
    /// no-op returning the source unchanged.
    pub fn remove_source_credential(&self, id: &str) -> Result<protocol::SourceView, ServiceError> {
        let source = {
            let g = lock(&self.inner);
            resolve_source(&g.store, id)?
        };
        let changed = source.cred_ref.is_some();
        if let Some(reference) = source.cred_ref {
            lock(&self.inner)
                .credentials
                .clone()
                .delete(&reference)
                .map_err(|e| ServiceError::Other(e.to_string()))?;
            lock(&self.inner)
                .store
                .clear_source_credential_ref(&source.id)
                .map_err(|e| ServiceError::Storage(e.to_string()))?;
        }
        let view = self
            .sources()?
            .into_iter()
            .find(|s| s.id == source.id)
            .ok_or_else(|| ServiceError::Rule(RuleError::UnknownSource(source.id.clone())))?;
        if changed {
            self.notify_changed();
        }
        Ok(view)
    }

    /// Delete a source, its still-pending suggestions, and its OS
    /// credential (#153), leaving a `kind="source"` tombstone (#172).
    /// Decided suggestions stay as history, and confirmed entries stay
    /// — they are tracked time, not suggestions. The credential purge is
    /// best-effort: when the store does not answer, the row (the app's
    /// only reference to the secret) is still removed, and the
    /// unreachable orphan is noted on stderr.
    pub fn remove_source(&self, id: &str) -> Result<(), ServiceError> {
        let source = {
            let g = lock(&self.inner);
            resolve_source(&g.store, id)?
        };
        if let Some(reference) = source.cred_ref
            && let Err(e) = lock(&self.inner).credentials.clone().delete(&reference)
        {
            eprintln!(
                "timetrack-service: removing source '{}' left its OS credential behind ({e}); \
                 delete it in your key manager",
                source.id
            );
        }
        let removed = lock(&self.inner)
            .store
            .remove_source(&source.id, now_ms())
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        if removed {
            self.notify_changed();
            Ok(())
        } else {
            Err(ServiceError::Rule(RuleError::UnknownSource(source.id)))
        }
    }

    /// Discover an account's CalDAV collections from a server,
    /// account, principal, or home URL (#150), so the user can pick
    /// collections without hand-constructing URLs. Read-only end to
    /// end: discovery sends `PROPFIND`/`GET` only and registers
    /// nothing — each found URL becomes a source through `add_source`.
    ///
    /// Authentication follows the fetch matrix (#149): `source_id`
    /// lends a registered source's stored credential (and username),
    /// falling back to `TIMETRACK_CALDAV_PASSWORD` only when that
    /// source holds none; without a source the endpoint's userinfo
    /// and the same env fallback apply.
    pub fn discover_caldav(
        &self,
        endpoint: &str,
        source_id: Option<&str>,
    ) -> Result<Vec<protocol::CaldavCollectionView>, ServiceError> {
        if endpoint.trim().is_empty() {
            return Err(ServiceError::Other(
                "discovery needs a server or account URL".into(),
            ));
        }
        // Refuses an embedded password, exactly as registration does.
        crate::caldav::split_userinfo(endpoint).map_err(ServiceError::Other)?;
        let (cred_ref, cred_username) = match source_id {
            Some(id) => {
                let source = resolve_source(&lock(&self.inner).store, id)?;
                (source.cred_ref, source.cred_username)
            }
            None => (None, None),
        };
        let credentials = lock(&self.inner).credentials.clone();
        let env_password = std::env::var("TIMETRACK_CALDAV_PASSWORD").ok();
        let auth = crate::credentials::resolve_caldav_auth(
            cred_ref.as_deref(),
            cred_username.as_deref(),
            endpoint,
            &|reference| credentials.get(reference),
            env_password.as_deref(),
        )
        .map_err(ServiceError::Other)?;
        let auth = auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
        // Off the state lock: discovery is network I/O, and the lock
        // is only needed to resolve the credential above.
        let found =
            crate::caldav::discover_collections(endpoint, auth).map_err(ServiceError::Other)?;
        Ok(found
            .into_iter()
            .map(|c| protocol::CaldavCollectionView {
                url: c.url,
                display_name: c.display_name,
                components: c.components,
            })
            .collect())
    }

    /// Set (or clear) an entry's free-form note (socket-only surface).
    pub fn set_note(&self, id: &str, note: Option<&str>) -> Result<EntryView, ServiceError> {
        let e = self.mutate(|s, _ids, _| core::set_text(s, id, None, Some(note)))?;
        Ok(protocol::entry_to_view(&e))
    }
}

/// Resolve an external source id or unique prefix against the store (#172).
fn resolve_source(
    store: &SqliteStore,
    want: &str,
) -> Result<timetrack_store::ExternalSource, ServiceError> {
    let all = store
        .sources()
        .map_err(|e| ServiceError::Storage(e.to_string()))?;
    let resolved = core::resolve_source_id(want, all.iter().map(|s| s.id.as_str()))?;
    all.into_iter()
        .find(|s| s.id == resolved)
        .ok_or_else(|| ServiceError::Rule(RuleError::UnknownSource(want.to_string())))
}

/// What registration and editing agree a source's identity must be
/// (#153): a known kind, no password smuggled into a CalDAV URL, and a
/// non-blank locator. One function so the two paths can never refuse
/// differently.
fn validate_source_identity(kind: &str, locator: &str) -> Result<(), ServiceError> {
    if kind != "ics_file" && kind != "ics_url" && kind != "caldav" {
        return Err(ServiceError::Other(format!(
            "unknown source kind '{kind}' (use ics_file, ics_url, or caldav)"
        )));
    }
    if kind == "caldav" {
        crate::caldav::split_userinfo(locator).map_err(ServiceError::Other)?;
    }
    if locator.trim().is_empty() {
        return Err(ServiceError::Other(
            "a source needs a file path or URL".into(),
        ));
    }
    Ok(())
}

fn source_view(s: timetrack_store::ExternalSource) -> protocol::SourceView {
    protocol::SourceView {
        id: s.id,
        kind: s.kind,
        locator: s.locator,
        project_id: s.project_id,
        label: s.label,
        has_credential: s.cred_ref.is_some(),
        username: s.cred_username,
    }
}

fn suggestion_view(s: timetrack_store::Suggestion) -> protocol::SuggestionView {
    protocol::SuggestionView {
        id: s.id,
        source_id: s.source_id,
        external_uid: s.external_uid,
        kind: s.kind,
        title: s.title,
        started_at: s.started_at,
        ended_at: s.ended_at,
        status: s.status,
        entry_id: s.entry_id,
    }
}

/// Read one source's calendar texts: a file from disk, an HTTPS GET
/// under a hard byte cap and a 15 s timeout (the update check's terms),
/// or every `.ics` resource in a CalDAV collection. Read-only by
/// construction — nothing here can write back.
///
/// A CalDAV source with a stored credential authenticates with it
/// (#149); one without falls back to `TIMETRACK_CALDAV_PASSWORD`, the
/// documented escape for headless setups.
fn fetch_source_texts(
    src: &timetrack_store::ExternalSource,
    credentials: &dyn CredentialStore,
) -> Result<Vec<String>, ServiceError> {
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    match src.kind.as_str() {
        "ics_file" => {
            let text = std::fs::read_to_string(&src.locator)
                .map_err(|e| ServiceError::Other(format!("could not read {}: {e}", src.locator)))?;
            if text.len() as u64 > MAX_BYTES {
                return Err(ServiceError::Other(format!(
                    "{} exceeds the {MAX_BYTES}-byte calendar cap",
                    src.locator
                )));
            }
            Ok(vec![text])
        }
        "caldav" => {
            if src.cred_ref.is_none() {
                return crate::caldav::fetch_calendar(&src.locator).map_err(ServiceError::Other);
            }
            let auth = crate::credentials::resolve_caldav_auth(
                src.cred_ref.as_deref(),
                src.cred_username.as_deref(),
                &src.locator,
                &|reference| credentials.get(reference),
                None,
            )
            .map_err(ServiceError::Other)?;
            let auth = auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
            crate::caldav::fetch_calendar_with_auth(&src.locator, auth).map_err(ServiceError::Other)
        }
        "ics_url" => {
            use std::io::Read;
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(15)))
                .build()
                .into();
            let mut response = agent
                .get(&src.locator)
                .call()
                .map_err(|e| ServiceError::Other(format!("fetch {} failed: {e}", src.locator)))?;
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| ServiceError::Other(format!("fetch {} failed: {e}", src.locator)))?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(ServiceError::Other(format!(
                    "{} exceeds the {MAX_BYTES}-byte calendar cap",
                    src.locator
                )));
            }
            String::from_utf8(bytes)
                .map(|t| vec![t])
                .map_err(|_| ServiceError::Other(format!("{} is not UTF-8 text", src.locator)))
        }
        other => Err(ServiceError::Other(format!(
            "unknown source kind '{other}'"
        ))),
    }
}

fn to_totals_view(t: &core::Totals) -> TotalsView {
    TotalsView {
        total_ms: t.total_ms,
        per_project: t.per_project.clone(),
        entry_count: t.entry_count,
    }
}

// Timezone resolution lives in `timetrack-core::Tz`: `Tz::try_from_env`
// reads `TZ` (or the system zone) strictly, `refresh_tz` re-reads it live
// (#102), and offsets are then resolved per instant, so DST transitions
// bucket correctly. There is deliberately no offset logic left here — one
// implementation, in the core, so both UIs agree.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The SQLite store for a legacy-shaped `store.json` test path: the
    /// database lands beside it as `store.db`, exactly as in production
    /// (§17), so `path` below still names the JSON name tests have always
    /// used for "the store over there".
    fn store_at(path: PathBuf) -> SqliteStore {
        SqliteStore::open_at(&path).unwrap()
    }

    fn service(name: &str) -> (EntryService, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tt-svc-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        (EntryService::new(store_at(path.clone())).unwrap(), path)
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
        let reopened = EntryService::new(store_at(path)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].description, "writing");
    }

    #[test]
    fn a_failed_save_rolls_the_mutation_back() {
        // #100: memory must match disk after a save failure, so the entry
        // the client was told failed must not linger in snapshots. Since
        // §17 the mutation path is one SQLite transaction, so the failure
        // is injected as a real write lock: a second connection holds an
        // open write transaction and SQLite itself refuses the service.
        let (svc, path) = service("save-failure-rollback");
        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "kept", now - 3_600_000, now).unwrap();

        let db_path = SqliteStore::db_path_for(&path);
        let mut locker = rusqlite::Connection::open(&db_path).unwrap();
        let lock_tx = locker.transaction().unwrap();
        lock_tx
            .execute(
                "INSERT INTO meta(key, value) VALUES ('lock_probe', '1')
                 ON CONFLICT(key) DO NOTHING",
                [],
            )
            .unwrap();
        let failed = svc.add(&p.id, "doomed", now - 1_800_000, now);
        drop(lock_tx);
        drop(locker);

        assert!(
            matches!(failed, Err(ServiceError::Storage(_))),
            "client must get a storage error, got: {failed:?}"
        );
        let snap = svc.snapshot();
        assert_eq!(snap.entries.len(), 1, "rolled-back entry must not show");
        assert_eq!(snap.entries[0].description, "kept");

        // Disk agrees, and the service still works once writes recover.
        let reopened = EntryService::new(store_at(path)).unwrap();
        assert_eq!(reopened.snapshot().entries.len(), 1);
        svc.add(&p.id, "after", now - 1_800_000, now).unwrap();
        assert_eq!(svc.snapshot().entries.len(), 2);
    }

    #[test]
    fn changed_subscribers_are_notified_only_by_successful_mutations() {
        // §16: the socket transport's Changed is notified from the
        // mutation funnel, so it fires once per persisted mutation and
        // never for a refusal -- on either transport, since both call
        // these same methods.
        let (svc, _) = service("changed-notify");
        let p = project(&svc);
        let rx = svc.subscribe_changed();
        let now = now_ms();
        svc.add(&p.id, "kept", now, now + 1).unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "a persisted mutation notifies");
        assert!(
            svc.add(&p.id, "bad", now + 1, now).is_err(),
            "setup: a refusal to not-notify with"
        );
        assert!(rx.try_recv().is_err(), "a refused mutation must not notify");
    }

    #[test]
    fn ids_do_not_collide_after_restart() {
        let (svc, path) = service("ids");
        let p = project(&svc);
        let now = now_ms();
        for i in 0..3 {
            svc.add(&p.id, &format!("t{i}"), now, now + 1).unwrap();
        }
        let reopened = EntryService::new(store_at(path)).unwrap();
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
    fn a_legacy_json_store_is_imported_on_first_start() {
        // §17: a v1 store (checkpoint + journal) is imported by the first
        // SQLite open, served identically, and left byte-identical on
        // disk as the backup/rollback -- including across restarts,
        // which must never re-import over newer database state.
        use timetrack_core::JsonStore as LegacyStore;
        let dir = std::env::temp_dir().join("tt-svc-legacy-import");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let legacy = LegacyStore::new(path.clone());
        let mut data = Store {
            version: core::STORE_VERSION,
            projects: vec![timetrack_core::Project {
                id: "p1".into(),
                name: "General".into(),
                colour: None,
                archived: false,
            }],
            entries: vec![],
        };
        legacy.save(&data).unwrap();
        data.entries.push(timetrack_core::Entry {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "from json".into(),
            started_at: 1_000,
            ended_at: 61_000,
            source: EntrySource::Manual,
            note: None,
        });
        let empty = Store {
            version: core::STORE_VERSION,
            projects: data.projects.clone(),
            entries: vec![],
        };
        legacy
            .append(&core::Journal::between(&empty, &data))
            .unwrap();
        let json_before = std::fs::read(&path).unwrap();
        let journal_before = std::fs::read(legacy.journal_path()).unwrap();

        let svc = EntryService::new(store_at(path.clone())).unwrap();
        let snap = svc.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].description, "from json");
        assert!(
            SqliteStore::db_path_for(&path).exists(),
            "the database is created beside the JSON store"
        );

        let p = project(&svc);
        let now = now_ms();
        svc.add(&p.id, "in sqlite", now, now + 1).unwrap();
        drop(svc);
        let reopened = EntryService::new(store_at(path.clone())).unwrap();
        assert_eq!(
            reopened.snapshot().entries.len(),
            2,
            "restart serves the database, never re-imports the JSON"
        );
        assert_eq!(std::fs::read(&path).unwrap(), json_before);
        assert_eq!(
            std::fs::read(legacy.journal_path()).unwrap(),
            journal_before
        );
    }

    #[test]
    fn a_missing_database_and_missing_json_is_a_fresh_usable_store() {
        // No import source and no database: the seed path must still
        // persist into SQLite, so a second start sees the seed project.
        // (No `store.json` is created any more.)
        let (svc, path) = service("fresh-sqlite");
        assert!(!path.exists(), "nothing writes store.json any more");
        let p = project(&svc);
        assert_eq!(p.name, "General");
        drop(svc);
        let reopened = EntryService::new(store_at(path)).unwrap();
        assert_eq!(reopened.snapshot().projects.len(), 1);
        assert_eq!(reopened.snapshot().projects[0].name, "General");
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
        let tz = core::Tz::from_snapshot(&snap0.tz).unwrap();
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
        let reopened = EntryService::new(store_at(path)).unwrap();
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
        let reopened = EntryService::new(store_at(path)).unwrap();
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
    fn blank_project_names_keep_the_core_rule_error() {
        use timetrack_core::RuleError;

        let (svc, _) = service("blank-project");
        let before = svc.snapshot().projects;
        assert_eq!(
            svc.add_project(" \t ").unwrap_err(),
            ServiceError::Rule(RuleError::BlankProjectName)
        );
        assert_eq!(svc.snapshot().projects, before);
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
        // -2 clears, and only -2 does: another negative still keeps (#104).
        assert_eq!(
            svc.update_project(&id, "", -7).unwrap().colour,
            Some(0x2ea043)
        );
        assert_eq!(svc.update_project(&id, "", -2).unwrap().colour, None);
        assert_eq!(svc.update_project(&id, "", -1).unwrap().colour, None);
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
        let tz = core::Tz::from_snapshot(&snap.tz).unwrap();
        assert_eq!(tz.name(), snap.tz.as_str());
        assert_eq!(tz.offset_at_ms(now_ms()), snap.local_offset_ms);
    }

    #[test]
    fn a_pinned_zone_survives_the_live_refresh() {
        // `new_with_tz` pins (#102): snapshots refresh a live service's
        // zone from the environment, but a pinned test zone must never be
        // replaced under the test, whatever the host zone is.
        let dir = std::env::temp_dir().join("tt-svc-tz-pinned");
        let _ = std::fs::remove_dir_all(&dir);
        let svc =
            EntryService::new_with_tz(store_at(dir.join("store.json")), core::Tz::utc()).unwrap();
        assert_eq!(svc.snapshot().tz, "UTC");
        assert_eq!(svc.snapshot().tz, "UTC");
    }

    #[test]
    fn client_parsed_times_and_service_buckets_agree_across_both_dst_transitions() {
        // #102 acceptance: the client path (snapshot zone strictly
        // resolved, typed HH:MM through the one core parser) and the
        // service path (totals/exports bucketed by the service's zone)
        // must put the same entry in the same local day and week, on
        // both Rome transition days.
        let dir = std::env::temp_dir().join("tt-svc-dst-agree");
        let _ = std::fs::remove_dir_all(&dir);
        let rome = core::Tz::parse("Europe/Rome").unwrap();
        let svc =
            EntryService::new_with_tz(store_at(dir.join("store.json")), rome.clone()).unwrap();
        let p = svc.snapshot().projects.first().cloned().unwrap();

        for (day, label) in [
            ("2026-03-29", "spring_entry"),
            ("2026-10-25", "autumn_entry"),
        ] {
            // What a client does: take the snapshot's zone, parse the
            // typed wall clocks as of that day.
            let snap = svc.snapshot();
            let client_tz = core::Tz::from_snapshot(&snap.tz).unwrap();
            assert_eq!(client_tz, rome);
            let now = core::parse_anchor_day(day, "anchor", &client_tz).unwrap();
            let start = core::parse_moment("00:30", "start", now, &client_tz).unwrap();
            let end = core::parse_moment("09:30", "end", now, &client_tz).unwrap();
            svc.add(&p.id, label, start, end).unwrap();

            // The service buckets it into that day's week, renders the
            // typed wall clocks back, and keeps it out of the prior week.
            let csv = svc.export_csv_at("week", now).unwrap();
            assert!(csv.contains(label), "{day} week export:\n{csv}");
            assert!(
                csv.contains(&format!("{day}T00:30:00")),
                "{day} start wall clock:\n{csv}"
            );
            assert!(
                csv.contains(&format!("{day}T09:30:00")),
                "{day} end wall clock:\n{csv}"
            );
            let prior = svc
                .export_csv_at("week", now - 7 * timetrack_core::MS_PER_DAY)
                .unwrap();
            assert!(!prior.contains(label), "{day} prior week:\n{prior}");
            assert_eq!(rome.day_of(start), rome.day_of(now));
        }
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
        let svc = EntryService::new_with_tz(store_at(path), rome.clone()).unwrap();
        assert_eq!(svc.snapshot().tz, "Europe/Rome");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        let utc_ms = 1_783_290_600_000; // 2026-07-05 22:30 UTC
        svc.add(&p.id, "late", utc_ms, utc_ms + 60_000).unwrap();

        let g = lock(&svc.inner);
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

    fn utc_service(name: &str) -> EntryService {
        let dir = std::env::temp_dir().join(format!("tt-svc-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        EntryService::new_with_tz(store_at(path), core::Tz::utc()).unwrap()
    }

    #[test]
    fn export_at_returns_the_chosen_week() {
        // ISO week 41 of 2026 runs Mon 5 Oct - Sun 11 Oct (UTC).
        let svc = utc_service("export-at-week");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        svc.add(&p.id, "mon", 1_791_194_400_000, 1_791_198_000_000)
            .unwrap();
        svc.add(&p.id, "sun", 1_791_712_800_000, 1_791_716_400_000)
            .unwrap();
        svc.add(&p.id, "next", 1_791_799_200_000, 1_791_802_800_000)
            .unwrap();
        let csv = svc.export_csv_at("week", 1_791_460_800_000).unwrap();
        assert!(csv.contains("mon"), "chosen week must hold Monday:\n{csv}");
        assert!(csv.contains("sun"), "chosen week must hold Sunday:\n{csv}");
        assert!(!csv.contains("next"), "next week must not leak in:\n{csv}");
    }

    #[test]
    fn export_at_returns_the_chosen_month() {
        let svc = utc_service("export-at-month");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        svc.add(&p.id, "sep", 1_789_466_400_000, 1_789_470_000_000)
            .unwrap();
        svc.add(&p.id, "oct", 1_790_848_800_000, 1_790_852_400_000)
            .unwrap();
        let sep = svc.export_csv_at("month", 1_789_473_600_000).unwrap();
        assert!(sep.contains("sep"), "September must hold its entry:\n{sep}");
        assert!(
            !sep.contains("oct"),
            "October must not leak into September:\n{sep}"
        );
        let oct = svc.export_csv_at("month", 1_791_460_800_000).unwrap();
        assert!(oct.contains("oct"), "October must hold its entry:\n{oct}");
        assert!(
            !oct.contains("sep"),
            "September must not leak into October:\n{oct}"
        );
    }

    #[test]
    fn export_at_all_ignores_the_anchor() {
        let svc = utc_service("export-at-all");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        svc.add(&p.id, "sep", 1_789_466_400_000, 1_789_470_000_000)
            .unwrap();
        svc.add(&p.id, "oct", 1_790_848_800_000, 1_790_852_400_000)
            .unwrap();
        let a = svc.export_csv_at("all", 1_789_473_600_000).unwrap();
        let b = svc.export_csv_at("all", 1_791_460_800_000).unwrap();
        assert_eq!(a, b, "a stale anchor must never narrow a full export");
    }

    #[test]
    fn export_at_rejects_an_unknown_scope_with_the_established_wording() {
        let svc = utc_service("export-at-scope");
        assert_eq!(
            svc.export_csv_at("everything", 0).unwrap_err(),
            ServiceError::UnknownExportScope("everything".into())
        );
        assert_eq!(
            svc.export_csv_at("everything", 0).unwrap_err().to_string(),
            "unknown export scope 'everything' (use week, month or all)"
        );
    }

    #[test]
    fn export_scope_only_keeps_current_period_behaviour() {
        // The old scope-only call is the now-anchored wrapper: for `all`
        // (which ignores the anchor) it must byte-match the anchored call.
        let svc = utc_service("export-compat");
        let p = svc.snapshot().projects.first().cloned().unwrap();
        svc.add(&p.id, "sep", 1_789_466_400_000, 1_789_470_000_000)
            .unwrap();
        assert_eq!(
            svc.export_csv("all").unwrap(),
            svc.export_csv_at("all", 1_791_460_800_000).unwrap()
        );
        assert_eq!(
            svc.export_csv("everything").unwrap_err().to_string(),
            "unknown export scope 'everything' (use week, month or all)"
        );
    }

    #[test]
    fn extreme_instants_are_refused_at_the_service_edge() {
        // #97/#98: raw i64s over the wire get the typed core refusal, fast,
        // and nothing is stored. All-time export keeps ignoring its anchor.
        let (svc, _) = service("extreme-instants");
        let p = project(&svc);
        assert_eq!(
            svc.add(&p.id, "x", 0, i64::MAX).unwrap_err(),
            ServiceError::Rule(RuleError::InstantOutOfRange(i64::MAX))
        );
        assert_eq!(
            svc.add_duration_ending(&p.id, "x", i64::MAX, i64::MIN)
                .unwrap_err(),
            ServiceError::Rule(RuleError::InstantOutOfRange(i64::MIN)),
            "the start subtraction is checked, not wrapped"
        );
        assert_eq!(
            svc.add_duration(&p.id, "x", i64::MIN).unwrap_err(),
            ServiceError::Rule(RuleError::InstantOutOfRange(i64::MAX))
        );
        for scope in ["week", "month"] {
            for anchor in [i64::MIN, i64::MAX] {
                assert_eq!(
                    svc.export_csv_at(scope, anchor).unwrap_err(),
                    ServiceError::Rule(RuleError::InstantOutOfRange(anchor)),
                    "{scope} anchor {anchor}"
                );
            }
        }
        assert!(
            svc.export_csv_at("all", i64::MAX).is_ok(),
            "all time ignores the anchor, extremes included"
        );
        assert!(svc.snapshot().entries.is_empty());
        // Boundary instants are accepted end to end.
        svc.add(&p.id, "edge", core::MIN_INSTANT_MS, core::MIN_INSTANT_MS)
            .unwrap();
        svc.add(&p.id, "edge", core::MAX_INSTANT_MS, core::MAX_INSTANT_MS)
            .unwrap();
        assert_eq!(svc.snapshot().entries.len(), 2);
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
        assert!(!snap.week.per_project.contains_key(&p.id));
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

    // --- UUID identities and prefix resolution (ADR 0018) ---

    #[test]
    fn the_seed_and_new_ids_are_uuids() {
        let (svc, _) = service("uuid-seed");
        let snap = svc.snapshot();
        assert_eq!(snap.projects.len(), 1);
        assert!(
            timetrack_store::is_uuid(&snap.projects[0].id),
            "the seed project is a UUID: {}",
            snap.projects[0].id
        );
        let p = project(&svc);
        let now = now_ms();
        let e = svc.add(&p.id, "x", now, now + 1).unwrap();
        assert!(
            timetrack_store::is_uuid(&e.id),
            "entries mint UUIDs: {}",
            e.id
        );
        let q = svc.add_project("Second").unwrap();
        assert!(
            timetrack_store::is_uuid(&q.id),
            "projects mint UUIDs: {}",
            q.id
        );
    }

    #[test]
    fn ids_minted_on_two_stores_never_collide() {
        // The point of ADR 0018: two stores creating independently must
        // not mint the same id, as the old counters did (both started
        // at e1/p1 and the seed was p1 on both).
        let (a, _) = service("uuid-a");
        let (b, _) = service("uuid-b");
        let pa = project(&a);
        let pb = project(&b);
        let now = now_ms();
        let mut ids = std::collections::HashSet::new();
        assert!(ids.insert(pa.id.clone()), "the seeds differ");
        assert!(ids.insert(pb.id.clone()), "the seeds differ");
        for (svc, p) in [(&a, &pa), (&b, &pb)] {
            for i in 0..25 {
                let e = svc.add(&p.id, &format!("t{i}"), now, now + 1).unwrap();
                assert!(ids.insert(e.id), "a minted entry id collided");
            }
            let q = svc.add_project("Extra").unwrap();
            assert!(ids.insert(q.id), "a minted project id collided");
        }
        assert_eq!(ids.len(), 2 + 2 * 26);
    }

    /// A store with pinned ids, so prefix tests never depend on random
    /// UUIDs sharing a prefix: `aaaaaaaa-*` projects and `bbbbbbbb-*`
    /// entries are ambiguous pairs, `cccccccc-*`/`dddddddd-*` unique.
    fn ambiguous_store() -> Store {
        use timetrack_core::Project;
        Store {
            version: core::STORE_VERSION,
            projects: vec![
                Project {
                    id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
                    name: "Work".into(),
                    colour: None,
                    archived: false,
                },
                Project {
                    id: "aaaaaaaa-0000-4000-8000-000000000002".into(),
                    name: "Home".into(),
                    colour: None,
                    archived: false,
                },
                Project {
                    id: "cccccccc-0000-4000-8000-000000000001".into(),
                    name: "Solo".into(),
                    colour: None,
                    archived: false,
                },
            ],
            entries: vec![
                timetrack_core::Entry {
                    id: "bbbbbbbb-0000-4000-8000-000000000001".into(),
                    project_id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
                    description: String::new(),
                    started_at: 0,
                    ended_at: 1,
                    source: EntrySource::Manual,
                    note: None,
                },
                timetrack_core::Entry {
                    id: "bbbbbbbb-0000-4000-8000-000000000002".into(),
                    project_id: "aaaaaaaa-0000-4000-8000-000000000002".into(),
                    description: String::new(),
                    started_at: 0,
                    ended_at: 1,
                    source: EntrySource::Manual,
                    note: None,
                },
                timetrack_core::Entry {
                    id: "dddddddd-0000-4000-8000-000000000001".into(),
                    project_id: "cccccccc-0000-4000-8000-000000000001".into(),
                    description: String::new(),
                    started_at: 0,
                    ended_at: 1,
                    source: EntrySource::Manual,
                    note: None,
                },
            ],
        }
    }

    /// A service over pinned ids (written verbatim, never minted).
    fn service_with(name: &str, data: &Store) -> EntryService {
        let dir = std::env::temp_dir().join(format!("tt-svc-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let store = store_at(path);
        store.save_all(data, 1_000).unwrap();
        EntryService::new(store).unwrap()
    }

    #[test]
    fn unique_prefixes_resolve_through_the_service() {
        let svc = service_with("prefix-ok", &ambiguous_store());
        let p = svc.update_project("cccccccc", "Renamed", -1).unwrap();
        assert_eq!(p.id, "cccccccc-0000-4000-8000-000000000001");
        let e = svc.set_text("dddddddd", "edited").unwrap();
        assert_eq!(e.id, "dddddddd-0000-4000-8000-000000000001");
        // Creation onto a project prefix attributes to the full id.
        let now = now_ms();
        let e = svc.add("cccccccc", "new", now, now + 1).unwrap();
        assert_eq!(e.project_id, "cccccccc-0000-4000-8000-000000000001");
        // Undo by prefix removes the prefixed entry. All but the last
        // character: short enough to prove the prefix path, long enough
        // to be collision-proof against the other random ids.
        let q = svc.quick_add("cccccccc", 5 * 60_000).unwrap();
        svc.undo_quick_add(&q.id[..q.id.len() - 1]).unwrap();
        assert!(!svc.snapshot().entries.iter().any(|x| x.id == q.id));
    }

    #[test]
    fn ambiguous_prefixes_refuse_without_mutating_or_notifying() {
        let svc = service_with("prefix-ambiguous", &ambiguous_store());
        let rx = svc.subscribe_changed();
        let before = svc.snapshot();
        assert_eq!(
            svc.set_text("bbbbbbbb", "x").unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousEntry("bbbbbbbb".into()))
        );
        assert_eq!(
            svc.update_project("aaaaaaaa", "X", -1).unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousProject("aaaaaaaa".into()))
        );
        assert_eq!(
            svc.add("aaaaaaaa", "x", 0, 1).unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousProject("aaaaaaaa".into()))
        );
        assert_eq!(
            svc.undo_quick_add("bbbbbbbb").unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousEntry("bbbbbbbb".into()))
        );
        assert_eq!(svc.snapshot(), before, "refusals change nothing");
        assert!(rx.try_recv().is_err(), "refusals notify nothing");
    }

    #[test]
    fn add_source_resolves_prefixes_and_refuses_ambiguous_before_writing() {
        let svc = service_with("prefix-source", &ambiguous_store());
        let src = svc
            .add_source("ics_file", "/tmp/cal.ics", Some("cccccccc"), "Cal")
            .unwrap();
        assert_eq!(
            src.project_id.as_deref(),
            Some("cccccccc-0000-4000-8000-000000000001")
        );
        assert_eq!(
            svc.add_source("ics_file", "/tmp/other.ics", Some("aaaaaaaa"), "Other")
                .unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousProject("aaaaaaaa".into()))
        );
        assert_eq!(
            svc.sources().unwrap().len(),
            1,
            "the refusal registered nothing"
        );
    }

    #[test]
    fn update_source_rewrites_and_reuses_registration_validation() {
        let svc = service_with("src-update", &ambiguous_store());
        let other = svc
            .add_source("ics_file", "/tmp/other.ics", None, "Other")
            .unwrap();
        let src = svc
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal")
            .unwrap();

        // Unknown id, bad kind, empty locator, CalDAV password in URL:
        // the same refusals registration gives.
        assert_eq!(
            svc.update_source("src-nope", "ics_file", "/tmp/x.ics", None, "X")
                .unwrap_err()
                .to_string(),
            "no external source with id 'src-nope'"
        );
        assert_eq!(
            svc.update_source(&src.id, "bogus", "/tmp/x.ics", None, "X")
                .unwrap_err()
                .to_string(),
            "unknown source kind 'bogus' (use ics_file, ics_url, or caldav)"
        );
        assert_eq!(
            svc.update_source(&src.id, "ics_file", "   ", None, "X")
                .unwrap_err()
                .to_string(),
            "a source needs a file path or URL"
        );
        assert!(
            svc.update_source(&src.id, "caldav", "https://u:p@cal.test/x/", None, "X")
                .unwrap_err()
                .to_string()
                .contains("do not put a password in the source URL"),
        );
        // ...plus the edit's own: another source's identity refuses.
        assert_eq!(
            svc.update_source(&src.id, "ics_file", "/tmp/other.ics", None, "X")
                .unwrap_err()
                .to_string(),
            "a ics_file source for '/tmp/other.ics' already exists"
        );
        // Its own identity is not a duplicate; an empty label re-derives.
        let updated = svc
            .update_source(
                &src.id,
                "ics_url",
                "https://cal.test/cal.ics",
                Some("cccccccc"),
                "",
            )
            .unwrap();
        assert_eq!(updated.id, src.id);
        assert_eq!(updated.kind, "ics_url");
        assert_eq!(updated.locator, "https://cal.test/cal.ics");
        assert_eq!(
            updated.project_id.as_deref(),
            Some("cccccccc-0000-4000-8000-000000000001")
        );
        assert_eq!(updated.label, "cal.ics");
        // The untouched source reads back unchanged.
        assert_eq!(
            svc.sources()
                .unwrap()
                .into_iter()
                .find(|s| s.id == other.id)
                .unwrap()
                .label,
            "Other"
        );
    }

    #[test]
    fn update_source_resolves_prefixes_and_refuses_ambiguous_before_writing() {
        let svc = service_with("src-update-prefix", &ambiguous_store());
        let src = svc
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal")
            .unwrap();
        assert_eq!(
            svc.update_source(&src.id, "ics_file", "/tmp/cal.ics", Some("aaaaaaaa"), "Cal")
                .unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousProject("aaaaaaaa".into()))
        );
        let kept = svc.sources().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].project_id, None, "the refusal mapped nothing");
    }

    #[test]
    fn source_mutations_notify_but_refusals_and_noops_do_not() {
        let svc = service_with("src-notify", &ambiguous_store());
        let rx = svc.subscribe_changed();
        let src = svc
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal")
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "a registration notifies");
        svc.add_source("ics_file", "/tmp/cal.ics", None, "Cal")
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "a duplicate registration persisted nothing, so it notifies nothing"
        );
        assert!(
            svc.update_source("src-nope", "ics_file", "/tmp/x.ics", None, "X")
                .is_err()
        );
        assert!(rx.try_recv().is_err(), "refusals notify nothing");
        svc.update_source(&src.id, "ics_file", "/tmp/cal.ics", None, "Renamed")
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "an edit notifies");
        svc.remove_source(&src.id).unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "a removal notifies");
        assert!(rx.try_recv().is_err(), "exactly three signals, no more");
    }

    #[test]
    fn confirm_resolves_its_project_before_claiming() {
        let svc = service_with("prefix-confirm", &ambiguous_store());
        let rx = svc.subscribe_changed();
        let src = svc
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal")
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "the registration notifies");
        let item = |uid: &str| timetrack_core::ExternalItem {
            uid: uid.into(),
            kind: timetrack_core::ExternalKind::Event,
            title: format!("Event {uid}"),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        let sg = {
            let g = lock(&svc.inner);
            g.store
                .upsert_suggestions(&src.id, &[item("ev1"), item("ev2")], 1_000)
                .unwrap();
            let pending = g.store.suggestions(true).unwrap();
            assert_eq!(pending.len(), 2);
            pending.into_iter().map(|s| s.id).collect::<Vec<_>>()
        };
        // An ambiguous override refuses with the suggestion still pending:
        // no claim, no entry, no notification.
        assert_eq!(
            svc.confirm_suggestion(&sg[0], Some("aaaaaaaa"))
                .unwrap_err(),
            ServiceError::Rule(RuleError::AmbiguousProject("aaaaaaaa".into()))
        );
        assert_eq!(svc.suggestions().unwrap().len(), 2);
        assert_eq!(svc.snapshot().entries.len(), 3);
        assert!(rx.try_recv().is_err(), "the refusal notifies nothing");
        // ...while a unique prefix confirms onto the resolved project.
        let e = svc.confirm_suggestion(&sg[0], Some("cccccccc")).unwrap();
        assert_eq!(e.project_id, "cccccccc-0000-4000-8000-000000000001");
        assert_eq!(e.source, protocol::EntrySource::External);
        assert_eq!(svc.suggestions().unwrap().len(), 1);
    }

    // --- source credentials (#149) ---

    use crate::credentials::FakeCredentialStore;

    fn service_with_fake(name: &str) -> (EntryService, PathBuf, Arc<FakeCredentialStore>) {
        let dir = std::env::temp_dir().join(format!("tt-svc-cred-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let fake = Arc::new(FakeCredentialStore::new());
        let svc = EntryService::new_with_credentials(store_at(path.clone()), fake.clone()).unwrap();
        (svc, path, fake)
    }

    /// Every byte SQLite holds for this store: the database plus its
    /// WAL, so a secret smuggled into any page or frame is caught.
    fn sqlite_bytes(json_path: &std::path::Path) -> Vec<u8> {
        let db_path = SqliteStore::db_path_for(json_path);
        let mut bytes = std::fs::read(&db_path).unwrap_or_default();
        bytes.extend(std::fs::read(db_path.with_extension("db-wal")).unwrap_or_default());
        bytes
    }

    #[test]
    fn credential_set_and_remove_keep_the_secret_out_of_sqlite() {
        let (svc, path, fake) = service_with_fake("round-trip");
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        assert!(!src.has_credential);

        let secret = "s3cr3t-per-source";
        let set = svc.set_source_credential(&src.id, "alice", secret).unwrap();
        assert!(set.has_credential);
        assert_eq!(set.username.as_deref(), Some("alice"));
        // The secret reached the credential store and only there.
        let reference = crate::credentials::credential_ref(&src.id);
        assert_eq!(fake.get(&reference).unwrap().as_deref(), Some(secret));
        let stored = lock(&svc.inner).store.source(&src.id).unwrap().unwrap();
        assert_eq!(stored.cred_ref.as_deref(), Some(reference.as_str()));
        assert_eq!(stored.cred_username.as_deref(), Some("alice"));
        for field in [
            stored.id.as_str(),
            stored.kind.as_str(),
            stored.locator.as_str(),
            stored.label.as_str(),
            stored.cred_ref.as_deref().unwrap_or_default(),
            stored.cred_username.as_deref().unwrap_or_default(),
        ] {
            assert!(
                !field.contains(secret),
                "no SQLite field may hold the secret: {field:?}"
            );
        }
        let bytes = sqlite_bytes(&path);
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the secret must not appear in any SQLite byte"
        );

        // Removing clears both the entry and the reference.
        let removed = svc.remove_source_credential(&src.id).unwrap();
        assert!(!removed.has_credential);
        assert_eq!(fake.get(&reference).unwrap(), None);
        let cleared = lock(&svc.inner).store.source(&src.id).unwrap().unwrap();
        assert_eq!(cleared.cred_ref, None);
        // Removing what was never set is a no-op, not an error.
        svc.remove_source_credential(&src.id).unwrap();
    }

    #[test]
    fn credential_changes_notify_but_a_noop_removal_does_not() {
        let (svc, _, _) = service_with_fake("cred-notify");
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        let rx = svc.subscribe_changed();
        svc.set_source_credential(&src.id, "alice", "s3cr3t")
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "storing notifies");
        svc.remove_source_credential(&src.id).unwrap();
        assert_eq!(rx.try_recv(), Ok(()), "clearing notifies");
        svc.remove_source_credential(&src.id).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "removing what was never set persisted nothing"
        );
    }

    #[test]
    fn credential_set_refuses_without_writing_anything() {
        let (svc, _, fake) = service_with_fake("refusals");
        assert!(
            svc.set_source_credential("src-nope", "alice", "s3cr3t")
                .is_err(),
            "an unknown source refuses"
        );
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        assert!(
            svc.set_source_credential(&src.id, "alice", "").is_err(),
            "an empty secret refuses"
        );
        // No username anywhere: the locator names none and none was given.
        assert!(
            svc.set_source_credential(&src.id, "", "s3cr3t").is_err(),
            "a missing username refuses"
        );
        // ...while a locator userinfo fills the gap.
        let with_user = svc
            .add_source("caldav", "https://bob@cal.test/u/", None, "Cal2")
            .unwrap();
        let set = svc
            .set_source_credential(&with_user.id, "", "s3cr3t")
            .unwrap();
        assert_eq!(set.username.as_deref(), Some("bob"));

        // A down store refuses with the database untouched: no dangling
        // reference, no half-written secret.
        fake.fail_with("daemon away");
        assert!(
            svc.set_source_credential(&src.id, "alice", "s3cr3t")
                .is_err()
        );
        let stored = lock(&svc.inner).store.source(&src.id).unwrap().unwrap();
        assert_eq!(stored.cred_ref, None);
        fake.heal();
        assert!(svc.remove_source_credential("src-nope").is_err());
    }

    #[test]
    fn credential_remove_keeps_the_reference_when_the_store_is_down() {
        let (svc, _, fake) = service_with_fake("remove-down");
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        svc.set_source_credential(&src.id, "alice", "s3cr3t")
            .unwrap();
        fake.fail_with("daemon away");
        // The delete failed, so the reference stays: clearing it would
        // silently downgrade the source to the env-var fallback.
        assert!(svc.remove_source_credential(&src.id).is_err());
        let stored = lock(&svc.inner).store.source(&src.id).unwrap().unwrap();
        assert!(stored.cred_ref.is_some());
        fake.heal();
        svc.remove_source_credential(&src.id).unwrap();
    }

    #[test]
    fn remove_source_purges_the_credential_and_its_suggestions() {
        let (svc, _, fake) = service_with_fake("remove");
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        svc.set_source_credential(&src.id, "alice", "s3cr3t")
            .unwrap();
        let reference = crate::credentials::credential_ref(&src.id);
        let item = |uid: &str| timetrack_core::ExternalItem {
            uid: uid.into(),
            kind: timetrack_core::ExternalKind::Event,
            title: format!("Event {uid}"),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        {
            let g = lock(&svc.inner);
            g.store
                .upsert_suggestions(&src.id, &[item("ev1"), item("ev2"), item("ev3")], 1_000)
                .unwrap();
        }
        // One decided each way before the removal: a confirmed entry
        // and a dismissal, beside the still-pending third.
        let pending = svc.suggestions().unwrap();
        assert_eq!(pending.len(), 3);
        let confirmed = svc.confirm_suggestion(&pending[0].id, None).unwrap();
        svc.dismiss_suggestion(&pending[1].id).unwrap();
        svc.remove_source(&src.id).unwrap();
        assert_eq!(fake.get(&reference).unwrap(), None, "the secret is purged");
        assert!(lock(&svc.inner).store.source(&src.id).unwrap().is_none());
        assert!(svc.suggestions().unwrap().is_empty());
        let decided = lock(&svc.inner).store.suggestions(false).unwrap();
        assert_eq!(decided.len(), 2, "decided rows survive the removal");
        assert!(
            svc.snapshot().entries.iter().any(|e| e.id == confirmed.id),
            "the confirmed entry is tracked time now, not a suggestion"
        );
        assert!(svc.remove_source(&src.id).is_err(), "twice refuses");

        // A down store does not hold deletion hostage: the row (the
        // app's only reference) still goes, orphaning at most an
        // unreachable entry.
        let src = svc
            .add_source("caldav", "https://cal.test/u2/", None, "Cal2")
            .unwrap();
        svc.set_source_credential(&src.id, "alice", "s3cr3t")
            .unwrap();
        fake.fail_with("daemon away");
        svc.remove_source(&src.id).unwrap();
        assert!(lock(&svc.inner).store.source(&src.id).unwrap().is_none());
    }

    #[test]
    fn refresh_authenticates_with_the_stored_per_source_credential() {
        use crate::caldav::fake::{Answer, multistatus, spawn};

        // Basic alice:s3cr3t-per-source, lowercased like the header
        // text it is matched against: only the stored secret is
        // accepted — no TIMETRACK_CALDAV_PASSWORD is involved.
        const BASIC: &str = "authorization: basic ywxpy2u6cznjcjn0lxblci1zb3vyy2u=";
        // The incremental shape: a CTag probe, an ETag listing, one GET.
        let (base, server) = spawn(3, |method, path, headers| {
            if !headers.to_ascii_lowercase().contains(BASIC) {
                return Answer::status_body("401 Unauthorized", "login first");
            }
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/cal/</d:href>\
                             <d:propstat><d:prop>\
                             <cs:getctag xmlns:cs=\"http://calendarserver.org/ns/\">ctag-1</cs:getctag>\
                             </d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                        ),
                    );
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/a.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"v1\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/a.ics" {
                let now = now_ms();
                let basic = |ms: i64| {
                    timetrack_core::format_local_iso8601(ms, &timetrack_core::Tz::utc())
                        .replace(['-', ':'], "")
                };
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), "\"v1\"".to_string())],
                    body: format!(
                        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ev-per-source\r\nSUMMARY:Per-source auth\r\nDTSTART:{}Z\r\nDTEND:{}Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                        basic(now - 1_800_000),
                        basic(now + 1_800_000),
                    ),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });

        let (svc, _, _) = service_with_fake("refresh-auth");
        let src = svc
            .add_source("caldav", &format!("{base}/cal/"), None, "Cal")
            .unwrap();
        // The locator names no user, so the stored username is the one
        // the server sees.
        svc.set_source_credential(&src.id, "alice", "s3cr3t-per-source")
            .unwrap();
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!((tally.fetched, tally.inserted), (1, 1));
        let log = server.join().unwrap();
        assert_eq!(log.len(), 3, "probe, listing, and one download");
        assert!(
            log.iter().all(|e| e.to_ascii_lowercase().contains(BASIC)),
            "every request authenticated"
        );
        let pending = svc.suggestions().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "Per-source auth");
    }

    #[test]
    fn refresh_with_a_dangling_reference_errors_actionably() {
        let (svc, _, fake) = service_with_fake("refresh-dangling");
        let src = svc
            .add_source("caldav", "https://cal.test/u/", None, "Cal")
            .unwrap();
        svc.set_source_credential(&src.id, "alice", "s3cr3t")
            .unwrap();
        // The OS entry vanishes out of band (another manager deleted
        // it): the refresh must say how to fix it, not silently fall
        // back to the environment.
        fake.delete(&crate::credentials::credential_ref(&src.id))
            .unwrap();
        let err = svc.refresh_source(Some(&src.id)).unwrap_err();
        assert!(
            err.to_string().contains("source-set-credential"),
            "the error says how to fix it: {err}"
        );
    }

    #[test]
    fn refresh_is_incremental_and_reconciles_remote_changes() {
        use crate::caldav::fake::{Answer, multistatus, spawn};
        use std::sync::{Arc, Mutex};

        // Server state the test mutates between refreshes.
        struct Resource {
            etag: &'static str,
            body: String,
        }
        struct Server {
            ctag: &'static str,
            resources: Vec<(&'static str, Resource)>,
        }
        let basic = |ms: i64| {
            timetrack_core::format_local_iso8601(ms, &timetrack_core::Tz::utc())
                .replace(['-', ':'], "")
        };
        let now = now_ms();
        let event = |uid: &str, summary: &str, start: i64, end: i64| {
            format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\nDTSTART:{}Z\r\nDTEND:{}Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                basic(start),
                basic(end)
            )
        };
        let state = Arc::new(Mutex::new(Server {
            ctag: "ctag-1",
            resources: vec![
                (
                    "a.ics",
                    Resource {
                        etag: "\"a1\"",
                        body: event("ev-a", "Alpha", now - 3_600_000, now - 1_800_000),
                    },
                ),
                (
                    "b.ics",
                    Resource {
                        etag: "\"b1\"",
                        body: event("ev-b", "Beta", now + 1_800_000, now + 3_600_000),
                    },
                ),
                (
                    "c.ics",
                    Resource {
                        etag: "\"c1\"",
                        body: event("ev-c", "Gamma", now + 5_400_000, now + 7_200_000),
                    },
                ),
            ],
        }));
        let route_state = state.clone();
        // r1: probe + listing + 3 GETs; r2: probe alone; r3: probe +
        // listing + GET b; r4: probe + listing. Eleven requests, 4 GETs.
        let (base, server) = spawn(11, move |method, path, headers| {
            let state = route_state.lock().unwrap();
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(&format!(
                            "<d:response><d:href>/cal/</d:href>\
                             <d:propstat><d:prop>\
                             <cs:getctag xmlns:cs=\"http://calendarserver.org/ns/\">\
                             {}</cs:getctag></d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                            state.ctag
                        )),
                    );
                }
                let mut inner = String::from(
                    "<d:response><d:href>/cal/</d:href>\
                     <d:propstat><d:prop><d:displayname>Cal</d:displayname></d:prop>\
                     <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                );
                for (name, resource) in &state.resources {
                    inner.push_str(&format!(
                        "<d:response><d:href>/cal/{name}</d:href>\
                         <d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                        resource.etag
                    ));
                }
                return Answer::status_body("207 Multi-Status", &multistatus(&inner));
            }
            if let Some(name) = path.strip_prefix("/cal/")
                && let Some((_, resource)) = state.resources.iter().find(|(n, _)| *n == name)
            {
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), resource.etag.to_string())],
                    body: resource.body.clone(),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });

        let (svc, _, _) = service_with_fake("refresh-incr");
        let src = svc
            .add_source("caldav", &format!("{base}/cal/"), None, "Cal")
            .unwrap();

        // r1: the full first fetch inserts three suggestions.
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!((tally.fetched, tally.inserted), (3, 3));
        assert_eq!(svc.suggestions().unwrap().len(), 3);

        // r2: nothing changed — the probe is the whole refresh.
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!(
            (
                tally.fetched,
                tally.inserted,
                tally.updated,
                tally.withdrawn
            ),
            (0, 0, 0, 0)
        );

        // r3: b is retitled remotely — its pending suggestion updates
        // in place instead of duplicating.
        {
            let mut state = state.lock().unwrap();
            state.ctag = "ctag-2";
            let b = state
                .resources
                .iter_mut()
                .find(|(n, _)| *n == "b.ics")
                .unwrap();
            b.1.etag = "\"b2\"";
            b.1.body = event("ev-b", "Beta rescheduled", now + 1_800_000, now + 3_600_000);
        }
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!((tally.fetched, tally.inserted, tally.updated), (1, 0, 1));
        let pending = svc.suggestions().unwrap();
        assert_eq!(pending.len(), 3);
        assert!(
            pending
                .iter()
                .any(|s| s.external_uid == "ev-b" && s.title == "Beta rescheduled"),
            "the retitle lands on the pending suggestion: {pending:?}"
        );

        // Decide two: a confirms into a real entry, b dismisses. Then
        // the server deletes a and c.
        let a_id = pending
            .iter()
            .find(|s| s.external_uid == "ev-a")
            .unwrap()
            .id
            .clone();
        let b_id = pending
            .iter()
            .find(|s| s.external_uid == "ev-b")
            .unwrap()
            .id
            .clone();
        svc.confirm_suggestion(&a_id, None).unwrap();
        svc.dismiss_suggestion(&b_id).unwrap();
        {
            let mut state = state.lock().unwrap();
            state.ctag = "ctag-3";
            state.resources.retain(|(n, _)| *n == "b.ics");
        }
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!(
            tally.withdrawn, 1,
            "only the still-pending deletion withdraws"
        );
        // The confirmed entry survives with its decision row; the
        // dismissal survives; the withdrawn pending row is gone.
        assert_eq!(svc.snapshot().entries.len(), 1);
        assert_eq!(svc.snapshot().entries[0].description, "Alpha");
        let all = lock(&svc.inner).store.suggestions(false).unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|s| s.external_uid == "ev-a" && s.status == "confirmed"),
            "the confirmed row survives its resource: {all:?}"
        );
        assert!(
            all.iter()
                .any(|s| s.external_uid == "ev-b" && s.status == "dismissed"),
            "the dismissal survives anything: {all:?}"
        );
        assert!(svc.suggestions().unwrap().is_empty());

        let log = server.join().unwrap();
        assert_eq!(log.len(), 11);
        assert_eq!(
            log.iter().filter(|e| e.starts_with("GET ")).count(),
            4,
            "three first-fetch downloads and one changed download: {log:?}"
        );
        assert!(
            log.iter().all(|e| {
                let line = e.lines().next().unwrap_or("");
                line.starts_with("PROPFIND ") || line.starts_with("GET ")
            }),
            "refresh sent only reads"
        );
    }

    #[test]
    fn refresh_recovers_from_a_dead_sync_token() {
        use crate::caldav::fake::{Answer, multistatus, spawn};
        use std::sync::{Arc, Mutex};

        let basic = |ms: i64| {
            timetrack_core::format_local_iso8601(ms, &timetrack_core::Tz::utc())
                .replace(['-', ':'], "")
        };
        let now = now_ms();
        struct Server {
            phase: u8,
            body: String,
        }
        // Phase 0 speaks sync-collection; in phase 1 the token died
        // and sync is gone, so the refresh must PROPFIND a full resync
        // instead of losing anything.
        let state = Arc::new(Mutex::new(Server {
            phase: 0,
            body: format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ev-tok\r\nSUMMARY:Token\r\nDTSTART:{}Z\r\nDTEND:{}Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                basic(now - 1_800_000),
                basic(now + 1_800_000),
            ),
        }));
        let route_state = state.clone();
        // r1: probe + REPORT + GET. r2: REPORT (410) + probe + listing
        // + GET.
        let (base, server) = spawn(7, move |method, path, headers| {
            let state = route_state.lock().unwrap();
            if method == "REPORT" && path == "/cal/" {
                if state.phase == 0 {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/cal/a.ics</d:href>\
                             <d:propstat><d:prop><d:getetag>\"t1\"</d:getetag></d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                             <d:sync-token>tok-1</d:sync-token>",
                        ),
                    );
                }
                return Answer::status_body("410 Gone", "unknown token");
            }
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    let reports = if state.phase == 0 {
                        "<d:supported-report-set><d:supported-report><d:report>\
                         <d:sync-collection/></d:report></d:supported-report>\
                         </d:supported-report-set>"
                    } else {
                        ""
                    };
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(&format!(
                            "<d:response><d:href>/cal/</d:href><d:propstat><d:prop>\
                             <cs:getctag xmlns:cs=\"http://calendarserver.org/ns/\">\
                             ctag-{}</cs:getctag>{}</d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                            state.phase + 1,
                            reports
                        )),
                    );
                }
                // The phase-1 listing: the same body, a rotated ETag.
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/a.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"t2\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/a.ics" {
                let etag = if state.phase == 0 { "\"t1\"" } else { "\"t2\"" };
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), etag.to_string())],
                    body: state.body.clone(),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });

        let (svc, _, _) = service_with_fake("refresh-token");
        let src = svc
            .add_source("caldav", &format!("{base}/cal/"), None, "Cal")
            .unwrap();
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!((tally.fetched, tally.inserted), (1, 1));
        {
            let g = lock(&svc.inner);
            let stored = g.store.caldav_sync_state(&src.id).unwrap().unwrap();
            assert_eq!(stored.sync_token.as_deref(), Some("tok-1"));
        }
        state.lock().unwrap().phase = 1;
        // The resync replays the same event: no duplicate, no loss —
        // and the dead token is replaced by the fresh CTag.
        let tally = svc.refresh_source(Some(&src.id)).unwrap();
        assert_eq!((tally.fetched, tally.inserted, tally.withdrawn), (0, 0, 0));
        assert_eq!(svc.suggestions().unwrap().len(), 1);
        {
            let g = lock(&svc.inner);
            let stored = g.store.caldav_sync_state(&src.id).unwrap().unwrap();
            assert_eq!(stored.sync_token, None);
            assert_eq!(stored.ctag.as_deref(), Some("ctag-2"));
            let resources = g.store.caldav_resources(&src.id).unwrap();
            assert_eq!(resources.len(), 1);
            assert_eq!(resources[0].etag.as_deref(), Some("\"t2\""));
        }
        let log = server.join().unwrap();
        assert_eq!(log.len(), 7);
        assert!(
            log[3].starts_with("REPORT /cal/") && log[3].contains("tok-1"),
            "the second refresh leads with the stored token: {}",
            log[3]
        );
        assert!(
            log.iter().all(|e| {
                let line = e.lines().next().unwrap_or("");
                line.starts_with("PROPFIND ")
                    || line.starts_with("GET ")
                    || line.starts_with("REPORT ")
            }),
            "refresh sent only reads"
        );
    }

    // --- CalDAV discovery (#150) ---

    #[test]
    fn discovery_lends_a_source_stored_credential() {
        use crate::caldav::fake::{Answer, multistatus, spawn};

        // Basic alice:s3cr3t-lent, lowercased like the header text it
        // is matched against: only the stored secret is accepted.
        const BASIC: &str = "authorization: basic ywxpy2u6cznjcjn0lwxlbnq=";
        // Two for the anonymous run, three for the authed one.
        let (base, server) = spawn(5, |method, path, headers| {
            if !headers.to_ascii_lowercase().contains(BASIC) {
                return Answer::status_body("401 Unauthorized", "login first");
            }
            if path == "/.well-known/caldav" {
                return Answer::status_body("404 Not Found", "no well-known");
            }
            if method == "PROPFIND" && path == "/dav/" {
                if headers.contains("Depth: 0") {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/dav/</d:href>\
                             <d:propstat><d:prop/>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                        ),
                    );
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/dav/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/dav/cal/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                         <d:displayname>Lent</d:displayname>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            Answer::status_body("404 Not Found", "no such path")
        });

        let (svc, _, _) = service_with_fake("discover-lent");
        // The locator names no user: the stored username is the one
        // the server sees, exactly as in refresh.
        let src = svc
            .add_source("caldav", &format!("{base}/dav/"), None, "Cal")
            .unwrap();
        svc.set_source_credential(&src.id, "alice", "s3cr3t-lent")
            .unwrap();
        // Without the source the fake 401s (no userinfo, no fallback
        // username); with it the stored pair authenticates.
        assert!(
            svc.discover_caldav(&format!("{base}/dav/"), None)
                .unwrap_err()
                .to_string()
                .contains("401")
        );
        let found = svc
            .discover_caldav(&format!("{base}/dav/"), Some(&src.id))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].display_name, "Lent");
        assert_eq!(found[0].url, format!("{base}/dav/cal/"));
        assert_eq!(found[0].components, ["VEVENT", "VTODO"]);
        // Discovery registers nothing and suggests nothing.
        assert_eq!(svc.sources().unwrap().len(), 1);
        assert!(svc.suggestions().unwrap().is_empty());
        let log = server.join().unwrap();
        assert_eq!(log.len(), 5);
        assert!(
            log.iter().all(|e| {
                let line = e.lines().next().unwrap_or("");
                line.starts_with("PROPFIND ") || line.starts_with("GET ")
            }),
            "discovery sent only reads"
        );
    }

    #[test]
    fn discovery_validates_before_touching_the_network() {
        let (svc, _, _) = service_with_fake("discover-refusals");
        assert!(svc.discover_caldav("", None).is_err(), "empty refuses");
        assert!(
            svc.discover_caldav("https://alice:secret@cal.test/u/", None)
                .unwrap_err()
                .to_string()
                .contains("source-set-credential"),
            "a locator password refuses"
        );
        assert!(
            svc.discover_caldav("https://cal.test/u/", Some("src-nope"))
                .unwrap_err()
                .to_string()
                .contains("src-nope"),
            "an unknown source refuses"
        );
    }

    #[test]
    fn source_id_prefix_resolution_works_and_ambiguous_refuses() {
        let dir = std::env::temp_dir().join("tt-svc-source-prefix");
        let _ = std::fs::remove_dir_all(&dir);
        let db_path = dir.join("store.db");
        {
            let store = SqliteStore::open_file(&db_path).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO external_sources(id, kind, locator, project_id, label, created_at, updated_at, device_id)
                 VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'caldav', 'https://cal.test/1/', NULL, 'One', 1, 1, 'dev'),
                        ('aaaaaaaa-0000-4000-8000-000000000002', 'caldav', 'https://cal.test/2/', NULL, 'Two', 1, 1, 'dev'),
                        ('cccccccc-0000-4000-8000-000000000001', 'ics_file', '/tmp/no-such-file.ics', NULL, 'Unique', 1, 1, 'dev')",
                [],
            )
            .unwrap();
            drop(store);
        }
        let fake = Arc::new(FakeCredentialStore::new());
        let svc = EntryService::new_with_credentials(
            SqliteStore::open_file(&db_path).unwrap(),
            fake.clone(),
        )
        .unwrap();

        // Ambiguous prefix refuses without mutating anything across all paths.
        for err in [
            svc.update_source("aaaaaaaa", "ics_file", "/tmp/x.ics", None, "X")
                .unwrap_err(),
            svc.set_source_credential("aaaaaaaa", "alice", "s3cr3t")
                .unwrap_err(),
            svc.remove_source_credential("aaaaaaaa").unwrap_err(),
            svc.remove_source("aaaaaaaa").unwrap_err(),
            svc.refresh_source(Some("aaaaaaaa")).unwrap_err(),
            svc.discover_caldav("https://cal.test/", Some("aaaaaaaa"))
                .unwrap_err(),
        ] {
            assert_eq!(
                err,
                ServiceError::Rule(RuleError::AmbiguousSource("aaaaaaaa".into()))
            );
        }
        assert_eq!(svc.sources().unwrap().len(), 3);

        // Unique prefix resolves cleanly on update, credential ops, and remove.
        let updated = svc
            .update_source("cccccccc", "caldav", "https://cal.test/3/", None, "Three")
            .unwrap();
        assert_eq!(updated.id, "cccccccc-0000-4000-8000-000000000001");
        assert_eq!(updated.label, "Three");

        let cred = svc
            .set_source_credential("cccccccc", "alice", "s3cr3t")
            .unwrap();
        assert!(cred.has_credential);
        assert_eq!(
            fake.get("caldav/cccccccc-0000-4000-8000-000000000001")
                .unwrap()
                .as_deref(),
            Some("s3cr3t")
        );
        let cleared = svc.remove_source_credential("cccccccc").unwrap();
        assert!(!cleared.has_credential);

        svc.remove_source("cccccccc").unwrap();
        assert_eq!(svc.sources().unwrap().len(), 2);
        let stones = lock(&svc.inner).store.tombstones().unwrap();
        assert!(
            stones
                .iter()
                .any(|t| { t.id == "cccccccc-0000-4000-8000-000000000001" && t.kind == "source" })
        );
    }
}
