/* store/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! SQLite persistence for TimeTrack (REQUIREMENTS §17, ADR 0014).
//!
//! The service is the only process that opens this database. Clients keep
//! talking to the service exactly as they did when the store was a JSON
//! checkpoint plus journal; what changed is only where the bytes live.
//! The core stays pure: its rules keep operating on the in-memory model,
//! and this crate translates between that model and rows.
//!
//! # Shape
//!
//! `projects` and `entries` mirror the model one row per record, plus the
//! sync metadata §20 asks for (`created_at`, `updated_at`, the writing
//! `device_id`) and a `tombstones` table so a deletion can later be said
//! out loud to a sync peer instead of merely being absent. A `seq`
//! autoincrement preserves the store's record order, which snapshot and
//! picker order have always followed.
//!
//! # Migration from JSON
//!
//! [`SqliteStore::open_at`] derives the database path from the legacy
//! `store.json` path (`store.db` beside it). When the database is newly
//! created and the JSON store exists, it is imported once -- replaying
//! the journal through the core's own loader, ids preserved, so the
//! result is verifiable entry-for-entry -- and the JSON files are left
//! untouched as backup and rollback. The core's `storage` module
//! therefore stays as the legacy import reader; nothing writes JSON any
//! more.

use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use timetrack_core::{
    Entry, EntrySource, ExternalItem, ExternalKind, Journal, JsonStore, Project, STORE_VERSION,
    Store,
};

/// The schema version this build writes, kept in `PRAGMA user_version`.
/// Bump with a numbered migration in [`migrate`]; a newer database is
/// refused, never silently reset (§17, the §2 rule).
pub const SCHEMA_VERSION: i64 = 7;

/// Whether `id` is already a UUID in hyphenated shape (ADR 0018):
/// 8-4-4-4-12 hex with hyphens in place. Either hex case counts — a
/// UUID by any case is still a UUID, and the remint leaves it alone
/// rather than rewriting an identity some peer may already hold.
pub fn is_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, c) in bytes.iter().enumerate() {
        let hyphen = i == 8 || i == 13 || i == 18 || i == 23;
        if hyphen {
            if *c != b'-' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Remint every non-UUID id in an in-memory store (the legacy JSON
/// import path, ADR 0018), so an imported store lands in the same
/// UUID-only state the v2→v3 migration produces.
///
/// References follow the mapping (entry→project); already-UUID ids
/// are left untouched, so mixed states converge without rewriting
/// identities that need no rewriting.
pub fn remint_store_ids(
    store: &mut Store,
    mint: &mut dyn FnMut() -> StoreResult<String>,
) -> StoreResult<()> {
    use std::collections::HashMap;
    let mut projects = HashMap::new();
    for p in &store.projects {
        if !is_uuid(&p.id) && !projects.contains_key(&p.id) {
            projects.insert(p.id.clone(), mint()?);
        }
    }
    let mut entries = HashMap::new();
    for e in &store.entries {
        if !is_uuid(&e.id) && !entries.contains_key(&e.id) {
            entries.insert(e.id.clone(), mint()?);
        }
    }
    for p in &mut store.projects {
        if let Some(new) = projects.get(&p.id) {
            p.id = new.clone();
        }
    }
    for e in &mut store.entries {
        if let Some(new) = entries.get(&e.id) {
            e.id = new.clone();
        }
        if let Some(new) = projects.get(&e.project_id) {
            e.project_id = new.clone();
        }
    }
    Ok(())
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not use the database at {path}: {source}")]
    Sqlite {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("{path} could not be read as a TimeTrack database: {reason}")]
    Invalid { path: String, reason: String },
    #[error(transparent)]
    Legacy(#[from] timetrack_core::StorageError),
    #[error("could not create {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub type StoreResult<T> = Result<T, StoreError>;

fn sqlite(path: &Path, source: rusqlite::Error) -> StoreError {
    StoreError::Sqlite {
        path: path.display().to_string(),
        source,
    }
}

/// Wall-clock now, for the one-time import stamp only.
fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A record a deletion removed, kept so a future sync can say "gone"
/// (§20) instead of the record simply not being there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub id: String,
    /// `"entry"`, `"project"`, or `"source"`.
    pub kind: String,
    pub deleted_at: i64,
    pub device_id: String,
}

/// The service's store: one SQLite database behind a mutex, mirroring the
/// persist-before-acknowledge contract the JSON journal had. A mutation
/// is one transaction; it is durable (`synchronous = FULL`) before the
/// service tells any client it happened.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    path: PathBuf,
    device_id: String,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// The database path belonging to a legacy JSON store path:
    /// `store.json` -> `store.db` beside it.
    pub fn db_path_for(json_path: &Path) -> PathBuf {
        json_path.with_extension("db")
    }

    /// Open (creating if needed) the store for the service.
    ///
    /// `json_path` names the legacy store location exactly as before
    /// this crate existed, so the service's own argument and the test
    /// rigs keep their meaning; the database lives beside it. On the
    /// first open -- no database file yet, JSON store present -- the
    /// JSON history is imported once (§17) and left untouched on disk.
    /// The import's metadata stamp reads the system clock here, at the
    /// persistence boundary; mutation stamps stay caller-supplied
    /// (`apply`/`save_all` take `now_ms`) so service tests stay exact.
    pub fn open_at(json_path: &Path) -> StoreResult<Self> {
        Self::open_at_with(json_path, &mut || Ok(uuid::Uuid::new_v4().to_string()))
    }

    fn open_at_with(
        json_path: &Path,
        mint: &mut dyn FnMut() -> StoreResult<String>,
    ) -> StoreResult<Self> {
        let db_path = Self::db_path_for(json_path);
        let existed = db_path.exists();
        if !existed && json_path.exists() {
            let mut legacy = JsonStore::new(json_path.to_path_buf()).load()?;
            // The JSON ids are sequential, but this database is v3+: remint
            // before opening/writing SQLite so a generator failure aborts cleanly
            // with neither a partial database nor modified JSON files (ADR 0018, #172).
            remint_store_ids(&mut legacy, mint)?;
            let store = Self::open_file(&db_path)?;
            store.save_all(&legacy, system_now_ms())?;
            store.set_meta("migrated_from", "store.json")?;
            return Ok(store);
        }
        Self::open_file(&db_path)
    }

    /// Open (creating if needed) a database at an explicit path, with no
    /// legacy import. Applies schema migrations; a database written by
    /// a newer build is refused.
    pub fn open_file(db_path: &Path) -> StoreResult<Self> {
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let mut conn = Connection::open(db_path).map_err(|e| sqlite(db_path, e))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| sqlite(db_path, e))?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|e| sqlite(db_path, e))?;
        migrate(&mut conn, db_path)?;
        let device_id = ensure_device_id(&conn, db_path)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
            path: db_path.to_path_buf(),
            device_id,
        })
    }

    /// Where the database lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This installation's stable device id (§20), minted on first open.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_meta(&self, key: &str, value: &str) -> StoreResult<()> {
        self.lock()
            .execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(())
    }

    /// Read the whole store, in record order. The service holds the
    /// model in memory exactly as it did over JSON; partial SQL queries
    /// for search/ranges arrive with the features that need them (§19).
    pub fn load(&self) -> StoreResult<Store> {
        let conn = self.lock();
        let mut projects = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, name, colour, archived FROM projects ORDER BY seq")
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        colour: row.get::<_, Option<i64>>(2)?.map(|c| c as u32),
                        archived: row.get::<_, i64>(3)? != 0,
                    })
                })
                .map_err(|e| sqlite(&self.path, e))?;
            for row in rows {
                projects.push(row.map_err(|e| sqlite(&self.path, e))?);
            }
        }
        let mut entries = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, project_id, description, started_at, ended_at, source, note
                     FROM entries ORDER BY seq",
                )
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    let source: i64 = row.get(5)?;
                    Ok((
                        Entry {
                            id: row.get(0)?,
                            project_id: row.get(1)?,
                            description: row.get(2)?,
                            started_at: row.get(3)?,
                            ended_at: row.get(4)?,
                            source: EntrySource::Manual,
                            note: row.get(6)?,
                        },
                        source,
                    ))
                })
                .map_err(|e| sqlite(&self.path, e))?;
            for row in rows {
                let (mut entry, source) = row.map_err(|e| sqlite(&self.path, e))?;
                entry.source = match source {
                    0 => EntrySource::Manual,
                    1 => EntrySource::QuickAdd,
                    2 => EntrySource::External,
                    other => {
                        return Err(StoreError::Invalid {
                            path: self.path.display().to_string(),
                            reason: format!("entry {} has unknown source {other}", entry.id),
                        });
                    }
                };
                entries.push(entry);
            }
        }
        Ok(Store {
            version: STORE_VERSION,
            projects,
            entries,
        })
    }

    /// Replace the whole contents with `store`, in one transaction.
    /// Used by the legacy import and the service's seed write; the
    /// mutation path uses [`apply`](Self::apply) so a tap stays one
    /// small transaction, like the journal line it replaces.
    pub fn save_all(&self, store: &Store, now_ms: i64) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        tx.execute("DELETE FROM entries", [])
            .and_then(|_| tx.execute("DELETE FROM projects", []))
            .map_err(|e| sqlite(&self.path, e))?;
        for project in &store.projects {
            upsert_project(&tx, project, now_ms, &self.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for entry in &store.entries {
            upsert_entry(&tx, entry, now_ms, &self.device_id).map_err(|e| sqlite(&self.path, e))?;
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Persist one mutation's diff (§17: persist before acknowledge).
    ///
    /// Upserts stamp `updated_at`/`device_id` (and `created_at` on
    /// insert); removals delete the row and leave a tombstone (§20).
    /// An empty diff writes nothing.
    pub fn apply(&self, diff: &Journal, now_ms: i64) -> StoreResult<()> {
        if diff.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        for id in &diff.removed_entries {
            tx.execute("DELETE FROM entries WHERE id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
            tombstone(&tx, id, "entry", now_ms, &self.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for entry in &diff.entries {
            upsert_entry(&tx, entry, now_ms, &self.device_id).map_err(|e| sqlite(&self.path, e))?;
        }
        for id in &diff.removed_projects {
            tx.execute("DELETE FROM projects WHERE id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
            tombstone(&tx, id, "project", now_ms, &self.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for project in &diff.projects {
            upsert_project(&tx, project, now_ms, &self.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Every tombstone, oldest deletion first (§20).
    pub fn tombstones(&self) -> StoreResult<Vec<Tombstone>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, deleted_at, device_id FROM tombstones
                 ORDER BY deleted_at, id",
            )
            .map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Tombstone {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    deleted_at: row.get(2)?,
                    device_id: row.get(3)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    // --- external sources and suggestions (§19) ---

    /// Register an external source, or return the existing one for the
    /// same `(kind, locator)`: adding a calendar twice is one source.
    pub fn add_source(
        &self,
        kind: &str,
        locator: &str,
        project_id: Option<&str>,
        label: &str,
        now_ms: i64,
    ) -> StoreResult<ExternalSource> {
        let conn = self.lock();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO external_sources(id, kind, locator, project_id, label, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
             ON CONFLICT(kind, locator) DO NOTHING",
            params![id, kind, locator, project_id, label, now_ms, self.device_id],
        )
        .map_err(|e| sqlite(&self.path, e))?;
        drop(conn);
        self.sources()?
            .into_iter()
            .find(|s| s.kind == kind && s.locator == locator)
            .ok_or_else(|| StoreError::Invalid {
                path: self.path.display().to_string(),
                reason: "source vanished during insert".into(),
            })
    }

    /// Every registered external source, in registration order.
    pub fn sources(&self) -> StoreResult<Vec<ExternalSource>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, locator, project_id, label, cred_ref, cred_username
                 FROM external_sources ORDER BY seq",
            )
            .map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ExternalSource {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    locator: row.get(2)?,
                    project_id: row.get(3)?,
                    label: row.get(4)?,
                    cred_ref: row.get(5)?,
                    cred_username: row.get(6)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    /// One source by id.
    pub fn source(&self, id: &str) -> StoreResult<Option<ExternalSource>> {
        Ok(self.sources()?.into_iter().find(|s| s.id == id))
    }

    /// Record a source's credential reference and login name (#149).
    /// The reference names the OS store entry; the username is
    /// non-secret display data. Neither is the secret, which never
    /// reaches this database. Returns false when the source is unknown.
    pub fn set_source_credential_ref(
        &self,
        id: &str,
        cred_ref: &str,
        username: &str,
    ) -> StoreResult<bool> {
        let changed = self
            .lock()
            .execute(
                "UPDATE external_sources SET cred_ref = ?2, cred_username = ?3 WHERE id = ?1",
                params![id, cred_ref, username],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(changed == 1)
    }

    /// Forget a source's credential reference (#149). Only the
    /// reference row goes; deleting the secret itself from the OS
    /// store is the service's job, done before this is called.
    /// Returns false when the source is unknown.
    pub fn clear_source_credential_ref(&self, id: &str) -> StoreResult<bool> {
        let changed = self
            .lock()
            .execute(
                "UPDATE external_sources SET cred_ref = NULL, cred_username = NULL WHERE id = ?1",
                params![id],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(changed == 1)
    }

    /// Rewrite a source's identity, mapping, and label (#153): kind,
    /// locator, project mapping, and label are all replaced wholesale —
    /// the caller (service, then GUI/CLI) sends the full new state, so
    /// there is no keep-vs-clear encoding on this seam.
    ///
    /// Still-pending and decided suggestions are untouched: the items
    /// stay actionable whatever the source is renamed to, and a
    /// dismissal stays a dismissal. Only the CalDAV sync state (#151)
    /// is conditional — a changed kind or locator invalidates the sync
    /// token and resource validators, so those go with the edit (the
    /// next refresh re-fetches whole), while a label/mapping-only edit
    /// keeps them. All in one transaction. Returns `None` when the
    /// source is unknown.
    pub fn update_source(
        &self,
        id: &str,
        kind: &str,
        locator: &str,
        project_id: Option<&str>,
        label: &str,
        now_ms: i64,
    ) -> StoreResult<Option<ExternalSource>> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT kind, locator FROM external_sources WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let Some((old_kind, old_locator)) = old else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE external_sources
             SET kind = ?2, locator = ?3, project_id = ?4, label = ?5,
                 updated_at = ?6, device_id = ?7
             WHERE id = ?1",
            params![id, kind, locator, project_id, label, now_ms, self.device_id],
        )
        .map_err(|e| sqlite(&self.path, e))?;
        if old_kind != kind || old_locator != locator {
            tx.execute("DELETE FROM caldav_sync WHERE source_id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
            tx.execute(
                "DELETE FROM caldav_resources WHERE source_id = ?1",
                params![id],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }
        let updated: ExternalSource = tx
            .query_row(
                "SELECT id, kind, locator, project_id, label, cred_ref, cred_username
                 FROM external_sources WHERE id = ?1",
                params![id],
                |row| {
                    Ok(ExternalSource {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        locator: row.get(2)?,
                        project_id: row.get(3)?,
                        label: row.get(4)?,
                        cred_ref: row.get(5)?,
                        cred_username: row.get(6)?,
                    })
                },
            )
            .map_err(|e| sqlite(&self.path, e))?;
        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(Some(updated))
    }

    /// Delete a source (#149, refined by #153, tombstoned in #172): its
    /// still-pending suggestions go with it, in the same transaction.
    /// Decided rows (confirmed/dismissed) stay as history, and confirmed
    /// entries stay — they are real tracked time now, not suggestions. The
    /// CalDAV sync state (#151) goes with the source, so a
    /// re-registered collection starts from a full fetch, not a stale
    /// token. When deleted, writes a `kind="source"` tombstone joining
    /// the same `(deleted_at, device_id)` ordering as entry/project
    /// tombstones. Returns false when the source was already unknown.
    pub fn remove_source(&self, id: &str, now_ms: i64) -> StoreResult<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        delete_source_rows(&tx, id).map_err(|e| sqlite(&self.path, e))?;
        let removed = tx
            .execute("DELETE FROM external_sources WHERE id = ?1", params![id])
            .map_err(|e| sqlite(&self.path, e))?;
        if removed == 1 {
            tombstone(&tx, id, "source", now_ms, &self.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(removed == 1)
    }

    // --- folder sync state, merge persistence, and conflict journal (#173) ---

    fn get_meta_in(conn: &Connection, path: &Path, key: &str) -> StoreResult<Option<String>> {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| sqlite(path, e))
    }

    fn set_meta_in(conn: &Connection, path: &Path, key: &str, value: &str) -> StoreResult<()> {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| sqlite(path, e))?;
        Ok(())
    }

    fn delete_meta_in(conn: &Connection, path: &Path, key: &str) -> StoreResult<()> {
        conn.execute("DELETE FROM meta WHERE key = ?1", params![key])
            .map_err(|e| sqlite(path, e))?;
        Ok(())
    }

    /// Current sync configuration and watermarks kept in `meta`.
    pub fn sync_config(&self) -> StoreResult<SyncConfig> {
        let conn = self.lock();
        let parse_i64 = |key: &str| -> StoreResult<Option<i64>> {
            Ok(Self::get_meta_in(&conn, &self.path, key)?.and_then(|v| v.parse::<i64>().ok()))
        };
        Ok(SyncConfig {
            folder: Self::get_meta_in(&conn, &self.path, "sync_folder")?,
            merged_through: parse_i64("sync_merged_through")?.unwrap_or(0),
            compacted_through: parse_i64("sync_compacted_through")?.unwrap_or(0),
            last_sync_at: parse_i64("sync_last_sync_at")?,
            last_error: Self::get_meta_in(&conn, &self.path, "sync_last_error")?,
            needs_repair: Self::get_meta_in(&conn, &self.path, "sync_needs_repair")?.as_deref()
                == Some("1"),
        })
    }

    /// Enable sync against `folder` (clearing any stale error/repair state).
    pub fn set_sync_folder(&self, folder: &str) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        Self::set_meta_in(&tx, &self.path, "sync_folder", folder)?;
        Self::delete_meta_in(&tx, &self.path, "sync_last_error")?;
        Self::delete_meta_in(&tx, &self.path, "sync_needs_repair")?;
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Record or clear `sync_last_error`, `sync_last_sync_at`, or `sync_needs_repair`.
    pub fn record_sync_status(
        &self,
        last_sync_at: Option<i64>,
        last_error: Option<Option<&str>>,
        needs_repair: Option<bool>,
    ) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        if let Some(ts) = last_sync_at {
            Self::set_meta_in(&tx, &self.path, "sync_last_sync_at", &ts.to_string())?;
        }
        if let Some(err_opt) = last_error {
            match err_opt {
                Some(msg) => Self::set_meta_in(&tx, &self.path, "sync_last_error", msg)?,
                None => Self::delete_meta_in(&tx, &self.path, "sync_last_error")?,
            }
        }
        if let Some(flag) = needs_repair {
            if flag {
                Self::set_meta_in(&tx, &self.path, "sync_needs_repair", "1")?;
            } else {
                Self::delete_meta_in(&tx, &self.path, "sync_needs_repair")?;
            }
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Unpair sync configuration and peer table without touching local
    /// entries, projects, sources, tombstones, or the conflict journal.
    pub fn clear_sync_config(&self) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        for key in [
            "sync_folder",
            "sync_merged_through",
            "sync_compacted_through",
            "sync_last_sync_at",
            "sync_last_error",
            "sync_needs_repair",
        ] {
            Self::delete_meta_in(&tx, &self.path, key)?;
        }
        tx.execute("DELETE FROM sync_peers", [])
            .map_err(|e| sqlite(&self.path, e))?;
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Read all syncable rows (projects, entries, sources, tombstones)
    /// with their full `(created_at, updated_at, device_id)` metadata.
    pub fn load_sync_rows(&self) -> StoreResult<SyncRows> {
        let conn = self.lock();
        let mut projects = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, colour, archived, created_at, updated_at, device_id
                     FROM projects ORDER BY seq",
                )
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(ProjectRow {
                        project: Project {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            colour: row.get::<_, Option<i64>>(2)?.map(|c| c as u32),
                            archived: row.get::<_, i64>(3)? != 0,
                        },
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                        device_id: row.get(6)?,
                    })
                })
                .map_err(|e| sqlite(&self.path, e))?;
            for row in rows {
                projects.push(row.map_err(|e| sqlite(&self.path, e))?);
            }
        }
        let mut entries = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, project_id, description, started_at, ended_at, source, note,
                            created_at, updated_at, device_id
                     FROM entries ORDER BY seq",
                )
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    let source_code: i64 = row.get(5)?;
                    Ok((
                        EntryRow {
                            entry: Entry {
                                id: row.get(0)?,
                                project_id: row.get(1)?,
                                description: row.get(2)?,
                                started_at: row.get(3)?,
                                ended_at: row.get(4)?,
                                source: EntrySource::Manual,
                                note: row.get(6)?,
                            },
                            external_source_id: None,
                            external_uid: None,
                            external_started_at: None,
                            created_at: row.get(7)?,
                            updated_at: row.get(8)?,
                            device_id: row.get(9)?,
                        },
                        source_code,
                    ))
                })
                .map_err(|e| sqlite(&self.path, e))?;
            for row in rows {
                let (mut er, code) = row.map_err(|e| sqlite(&self.path, e))?;
                er.entry.source = match code {
                    0 => EntrySource::Manual,
                    1 => EntrySource::QuickAdd,
                    2 => EntrySource::External,
                    other => {
                        return Err(StoreError::Invalid {
                            path: self.path.display().to_string(),
                            reason: format!("entry {} has unknown source {other}", er.entry.id),
                        });
                    }
                };
                entries.push(er);
            }
        }
        let mut sources = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, kind, locator, project_id, label, cred_ref, cred_username,
                            created_at, updated_at, device_id
                     FROM external_sources ORDER BY seq",
                )
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(ExternalSourceRow {
                        source: ExternalSource {
                            id: row.get(0)?,
                            kind: row.get(1)?,
                            locator: row.get(2)?,
                            project_id: row.get(3)?,
                            label: row.get(4)?,
                            cred_ref: row.get(5)?,
                            cred_username: row.get(6)?,
                        },
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                        device_id: row.get(9)?,
                    })
                })
                .map_err(|e| sqlite(&self.path, e))?;
            for row in rows {
                sources.push(row.map_err(|e| sqlite(&self.path, e))?);
            }
        }
        drop(conn);
        let tombstones = self.tombstones()?;
        Ok(SyncRows {
            projects,
            entries,
            sources,
            tombstones,
        })
    }

    /// Apply an entire sync merge atomically in a single SQLite transaction.
    /// Returns any local OS credential references (`cred_ref`) belonging to
    /// deleted sources so the service can purge them best-effort off-lock.
    pub fn apply_sync_merge(&self, write: &SyncMergeWrite) -> StoreResult<Vec<String>> {
        use rusqlite::OptionalExtension;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let mut purged_cred_refs = Vec::new();

        for id in &write.delete_sources {
            let cred_ref: Option<String> = tx
                .query_row(
                    "SELECT cred_ref FROM external_sources WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| sqlite(&self.path, e))?
                .flatten();
            if let Some(reference) = cred_ref {
                purged_cred_refs.push(reference);
            }
            delete_source_rows(&tx, id).map_err(|e| sqlite(&self.path, e))?;
            tx.execute("DELETE FROM external_sources WHERE id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for id in &write.delete_entries {
            tx.execute("DELETE FROM entries WHERE id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for id in &write.delete_projects {
            tx.execute("DELETE FROM projects WHERE id = ?1", params![id])
                .map_err(|e| sqlite(&self.path, e))?;
        }

        for pr in &write.upsert_projects {
            tx.execute(
                "INSERT INTO projects(id, name, colour, archived, created_at, updated_at, device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, colour = excluded.colour,
                     archived = excluded.archived,
                     created_at = CASE WHEN projects.created_at > 0 AND projects.created_at < excluded.created_at THEN projects.created_at ELSE excluded.created_at END,
                     updated_at = excluded.updated_at, device_id = excluded.device_id",
                params![
                    pr.project.id,
                    pr.project.name,
                    pr.project.colour.map(|c| c as i64),
                    pr.project.archived as i64,
                    pr.created_at,
                    pr.updated_at,
                    pr.device_id
                ],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        for er in &write.upsert_entries {
            tx.execute(
                "INSERT INTO entries(id, project_id, description, started_at, ended_at, source, note,
                     created_at, updated_at, device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET project_id = excluded.project_id,
                     description = excluded.description, started_at = excluded.started_at,
                     ended_at = excluded.ended_at, source = excluded.source, note = excluded.note,
                     created_at = CASE WHEN entries.created_at > 0 AND entries.created_at < excluded.created_at THEN entries.created_at ELSE excluded.created_at END,
                     updated_at = excluded.updated_at, device_id = excluded.device_id",
                params![
                    er.entry.id,
                    er.entry.project_id,
                    er.entry.description,
                    er.entry.started_at,
                    er.entry.ended_at,
                    source_code(er.entry.source),
                    er.entry.note,
                    er.created_at,
                    er.updated_at,
                    er.device_id
                ],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        // Park any existing source whose `(kind, locator)` is changing in
        // this batch so a two-source swap cannot trip `UNIQUE(kind, locator)`
        // mid-loop; clear its CalDAV cache while we still see the old key.
        for sr in &write.upsert_sources {
            let prev: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind, locator FROM external_sources WHERE id = ?1",
                    params![sr.source.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| sqlite(&self.path, e))?;
            if let Some((old_kind, old_locator)) = prev
                && (old_kind != sr.source.kind || old_locator != sr.source.locator)
            {
                tx.execute(
                    "DELETE FROM caldav_sync WHERE source_id = ?1",
                    params![sr.source.id],
                )
                .map_err(|e| sqlite(&self.path, e))?;
                tx.execute(
                    "DELETE FROM caldav_resources WHERE source_id = ?1",
                    params![sr.source.id],
                )
                .map_err(|e| sqlite(&self.path, e))?;
                let swap_locator = format!("__timetrack_sync_swap_{}", sr.source.id);
                tx.execute(
                    "UPDATE external_sources SET locator = ?1 WHERE id = ?2",
                    params![swap_locator, sr.source.id],
                )
                .map_err(|e| sqlite(&self.path, e))?;
            }
        }

        for sr in &write.upsert_sources {
            // Note: `cred_ref` and `cred_username` are deliberately omitted
            // from the upsert so local OS credential pointers never overwrite
            // or travel across devices.
            tx.execute(
                "INSERT INTO external_sources(id, kind, locator, project_id, label,
                     created_at, updated_at, device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, locator = excluded.locator,
                     project_id = excluded.project_id, label = excluded.label,
                     created_at = CASE WHEN external_sources.created_at > 0 AND external_sources.created_at < excluded.created_at THEN external_sources.created_at ELSE excluded.created_at END,
                     updated_at = excluded.updated_at, device_id = excluded.device_id",
                params![
                    sr.source.id,
                    sr.source.kind,
                    sr.source.locator,
                    sr.source.project_id,
                    sr.source.label,
                    sr.created_at,
                    sr.updated_at,
                    sr.device_id
                ],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        for t in &write.upsert_tombstones {
            tombstone(&tx, &t.id, &t.kind, t.deleted_at, &t.device_id)
                .map_err(|e| sqlite(&self.path, e))?;
        }
        for (id, kind) in &write.delete_tombstones {
            tx.execute(
                "DELETE FROM tombstones WHERE id = ?1 AND kind = ?2",
                params![id, kind],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        for c in &write.new_conflicts {
            tx.execute(
                "INSERT INTO sync_conflicts(id, kind, record_id, recorded_at,
                     loser_updated_at, loser_device_id, winner_updated_at,
                     winner_device_id, loser_payload, restored_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(kind, record_id, loser_updated_at, loser_device_id) DO NOTHING",
                params![
                    c.id,
                    c.kind,
                    c.record_id,
                    c.recorded_at,
                    c.loser_updated_at,
                    c.loser_device_id,
                    c.winner_updated_at,
                    c.winner_device_id,
                    c.loser_payload,
                    c.restored_at
                ],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        for p in &write.upsert_peers {
            tx.execute(
                "INSERT INTO sync_peers(device_id, last_published_at, last_merged_through,
                     last_seen_at, quarantined, last_warning)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(device_id) DO UPDATE SET last_published_at = excluded.last_published_at,
                     last_merged_through = excluded.last_merged_through,
                     last_seen_at = excluded.last_seen_at,
                     quarantined = excluded.quarantined,
                     last_warning = excluded.last_warning",
                params![
                    p.device_id,
                    p.last_published_at,
                    p.last_merged_through,
                    p.last_seen_at,
                    p.quarantined as i64,
                    p.last_warning
                ],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }
        for device_id in &write.delete_peers {
            tx.execute(
                "DELETE FROM sync_peers WHERE device_id = ?1",
                params![device_id],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        }

        if let Some(mt) = write.merged_through {
            Self::set_meta_in(&tx, &self.path, "sync_merged_through", &mt.to_string())?;
        }
        if let Some(ct) = write.compacted_through {
            Self::set_meta_in(&tx, &self.path, "sync_compacted_through", &ct.to_string())?;
        }
        if let Some(ts) = write.last_sync_at {
            Self::set_meta_in(&tx, &self.path, "sync_last_sync_at", &ts.to_string())?;
        }
        if let Some(ref err_opt) = write.last_error {
            match err_opt {
                Some(msg) => Self::set_meta_in(&tx, &self.path, "sync_last_error", msg)?,
                None => Self::delete_meta_in(&tx, &self.path, "sync_last_error")?,
            }
        }
        if let Some(flag) = write.needs_repair {
            if flag {
                Self::set_meta_in(&tx, &self.path, "sync_needs_repair", "1")?;
            } else {
                Self::delete_meta_in(&tx, &self.path, "sync_needs_repair")?;
            }
        }

        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(purged_cred_refs)
    }

    /// Every conflict journal row, newest recorded first.
    pub fn sync_conflicts(&self, include_restored: bool) -> StoreResult<Vec<SyncConflictRow>> {
        let conn = self.lock();
        let sql = if include_restored {
            "SELECT id, kind, record_id, recorded_at, loser_updated_at, loser_device_id,
                    winner_updated_at, winner_device_id, loser_payload, restored_at
             FROM sync_conflicts ORDER BY recorded_at DESC, id"
        } else {
            "SELECT id, kind, record_id, recorded_at, loser_updated_at, loser_device_id,
                    winner_updated_at, winner_device_id, loser_payload, restored_at
             FROM sync_conflicts WHERE restored_at IS NULL ORDER BY recorded_at DESC, id"
        };
        let mut stmt = conn.prepare(sql).map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SyncConflictRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    record_id: row.get(2)?,
                    recorded_at: row.get(3)?,
                    loser_updated_at: row.get(4)?,
                    loser_device_id: row.get(5)?,
                    winner_updated_at: row.get(6)?,
                    winner_device_id: row.get(7)?,
                    loser_payload: row.get(8)?,
                    restored_at: row.get(9)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    /// Re-save a losing record from the conflict journal with fresh
    /// `(updated_at = now_ms, device_id = self.device_id)` so it wins the
    /// next merge, remove any superseded local tombstone, and mark the
    /// journal entry restored.
    pub fn restore_sync_conflict(
        &self,
        conflict_id: &str,
        now_ms: i64,
        restored: &RestoredRecord,
    ) -> StoreResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let (kind, record_id): (String, String) = tx
            .query_row(
                "SELECT kind, record_id FROM sync_conflicts
                 WHERE id = ?1 AND restored_at IS NULL",
                params![conflict_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| sqlite(&self.path, e))?;
        match restored {
            RestoredRecord::Project(p) => {
                upsert_project(&tx, p, now_ms, &self.device_id)
                    .map_err(|e| sqlite(&self.path, e))?;
            }
            RestoredRecord::Entry { entry, .. } => {
                upsert_entry(&tx, entry, now_ms, &self.device_id)
                    .map_err(|e| sqlite(&self.path, e))?;
            }
            RestoredRecord::Source {
                id,
                kind: src_kind,
                locator,
                project_id,
                label,
            } => {
                tx.execute(
                    "INSERT INTO external_sources(id, kind, locator, project_id, label,
                         created_at, updated_at, device_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
                     ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, locator = excluded.locator,
                         project_id = excluded.project_id, label = excluded.label,
                         updated_at = excluded.updated_at, device_id = excluded.device_id",
                    params![
                        id,
                        src_kind,
                        locator,
                        project_id,
                        label,
                        now_ms,
                        self.device_id
                    ],
                )
                .map_err(|e| sqlite(&self.path, e))?;
            }
        }
        tx.execute(
            "DELETE FROM tombstones WHERE id = ?1 AND kind = ?2 AND deleted_at < ?3",
            params![record_id, kind, now_ms],
        )
        .map_err(|e| sqlite(&self.path, e))?;
        tx.execute(
            "UPDATE sync_conflicts SET restored_at = ?2 WHERE id = ?1",
            params![conflict_id, now_ms],
        )
        .map_err(|e| sqlite(&self.path, e))?;
        tx.commit().map_err(|e| sqlite(&self.path, e))
    }

    /// Tracked peer devices in `sync_peers`, ordered by `device_id`.
    pub fn sync_peers(&self) -> StoreResult<Vec<SyncPeerRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT device_id, last_published_at, last_merged_through,
                        last_seen_at, quarantined, last_warning
                 FROM sync_peers ORDER BY device_id",
            )
            .map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SyncPeerRow {
                    device_id: row.get(0)?,
                    last_published_at: row.get(1)?,
                    last_merged_through: row.get(2)?,
                    last_seen_at: row.get(3)?,
                    quarantined: row.get::<_, i64>(4)? != 0,
                    last_warning: row.get(5)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    /// Store freshly fetched items as pending suggestions. An item
    /// already known (same source, UID, and start) is left exactly as
    /// it was — a dismissed suggestion stays dismissed across fetches.
    /// Returns how many were newly inserted.
    pub fn upsert_suggestions(
        &self,
        source_id: &str,
        items: &[ExternalItem],
        now_ms: i64,
    ) -> StoreResult<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let mut inserted = 0;
        for item in items {
            let id = mint_id(&tx, "sg-").map_err(|e| sqlite(&self.path, e))?;
            let kind = match item.kind {
                ExternalKind::Event => "event",
                ExternalKind::Task => "task",
            };
            inserted += tx
                .execute(
                    "INSERT INTO suggestions(id, source_id, external_uid, kind, title, started_at, ended_at, fetched_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(source_id, external_uid, started_at) DO NOTHING",
                    params![
                        id,
                        source_id,
                        item.uid,
                        kind,
                        item.title,
                        item.started_at,
                        item.ended_at,
                        now_ms
                    ],
                )
                .map_err(|e| sqlite(&self.path, e))?;
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(inserted)
    }

    /// Suggestions oldest first; `pending_only` is the ordinary view.
    pub fn suggestions(&self, pending_only: bool) -> StoreResult<Vec<Suggestion>> {
        let conn = self.lock();
        let sql = if pending_only {
            "SELECT id, source_id, external_uid, kind, title, started_at, ended_at, status, entry_id
             FROM suggestions WHERE status = 'pending' ORDER BY started_at, seq"
        } else {
            "SELECT id, source_id, external_uid, kind, title, started_at, ended_at, status, entry_id
             FROM suggestions ORDER BY started_at, seq"
        };
        let mut stmt = conn.prepare(sql).map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Suggestion {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    external_uid: row.get(2)?,
                    kind: row.get(3)?,
                    title: row.get(4)?,
                    started_at: row.get(5)?,
                    ended_at: row.get(6)?,
                    status: row.get(7)?,
                    entry_id: row.get(8)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    /// One suggestion by id.
    pub fn suggestion(&self, id: &str) -> StoreResult<Option<Suggestion>> {
        Ok(self.suggestions(false)?.into_iter().find(|s| s.id == id))
    }

    /// Decide a pending suggestion. Returns false when it was not
    /// pending (unknown, or already decided): deciding twice is a
    /// no-op, never a second entry.
    /// Attach the created entry to a confirmed suggestion (the service
    /// claims the suggestion first, then creates the entry, then links
    /// it — so a racing second confirm can never create a duplicate).
    pub fn link_suggestion_entry(&self, id: &str, entry_id: &str) -> StoreResult<()> {
        self.lock()
            .execute(
                "UPDATE suggestions SET entry_id = ?2 WHERE id = ?1",
                params![id, entry_id],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(())
    }

    pub fn decide_suggestion(
        &self,
        id: &str,
        status: &str,
        entry_id: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<bool> {
        let changed = self
            .lock()
            .execute(
                "UPDATE suggestions SET status = ?2, entry_id = ?3, decided_at = ?4
                 WHERE id = ?1 AND status = 'pending'",
                params![id, status, entry_id, now_ms],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(changed == 1)
    }

    // --- CalDAV incremental sync state (#151, schema v5) ---
    //
    // What a refresh needs to avoid re-downloading an unchanged
    // collection: per-source sync state (the sync token and/or CTag)
    // and per-resource validators (the ETag, or a content hash when the
    // server sends none). Suggestions fetched from CalDAV also carry
    // the resource they came from, so a changed or deleted resource
    // reconciles exactly its own suggestions.

    /// The stored sync state for a source: `None` when it never synced.
    pub fn caldav_sync_state(&self, source_id: &str) -> StoreResult<Option<CaldavSyncState>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT sync_token, ctag FROM caldav_sync WHERE source_id = ?1")
            .map_err(|e| sqlite(&self.path, e))?;
        let mut rows = stmt
            .query_map(params![source_id], |row| {
                Ok(CaldavSyncState {
                    sync_token: row.get(0)?,
                    ctag: row.get(1)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(|e| sqlite(&self.path, e))?)),
            None => Ok(None),
        }
    }

    /// Record a source's sync state after a refresh applied its data.
    /// The service calls this last, once every resource reconciliation
    /// and withdrawal is stored: the token/CTag claims "seen it all",
    /// so it must never get ahead of the suggestions it describes.
    pub fn set_caldav_sync_state(
        &self,
        source_id: &str,
        sync_token: Option<&str>,
        ctag: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<()> {
        self.lock()
            .execute(
                "INSERT INTO caldav_sync(source_id, sync_token, ctag, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_id) DO UPDATE SET sync_token = excluded.sync_token,
                     ctag = excluded.ctag, updated_at = excluded.updated_at",
                params![source_id, sync_token, ctag, now_ms],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(())
    }

    /// Every known resource of a source with its validators, in href
    /// order. Empty for a source that never synced.
    pub fn caldav_resources(&self, source_id: &str) -> StoreResult<Vec<CaldavResource>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT href, etag, content_hash FROM caldav_resources
                 WHERE source_id = ?1 ORDER BY href",
            )
            .map_err(|e| sqlite(&self.path, e))?;
        let rows = stmt
            .query_map(params![source_id], |row| {
                Ok(CaldavResource {
                    href: row.get(0)?,
                    etag: row.get(1)?,
                    content_hash: row.get(2)?,
                })
            })
            .map_err(|e| sqlite(&self.path, e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite(&self.path, e))?);
        }
        Ok(out)
    }

    /// Remember a resource's validators after its body was applied.
    pub fn upsert_caldav_resource(
        &self,
        source_id: &str,
        href: &str,
        etag: Option<&str>,
        content_hash: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<()> {
        self.lock()
            .execute(
                "INSERT INTO caldav_resources(source_id, href, etag, content_hash, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(source_id, href) DO UPDATE SET etag = excluded.etag,
                     content_hash = excluded.content_hash, seen_at = excluded.seen_at",
                params![source_id, href, etag, content_hash, now_ms],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        Ok(())
    }

    /// Reconcile one freshly fetched resource against its pending
    /// suggestions, in one transaction:
    ///
    /// - new occurrences are inserted — unless a decided row already
    ///   holds their identity, which keeps a dismissed suggestion
    ///   dismissed (and a confirmed one confirmed) across re-fetches;
    /// - a still-pending occurrence whose visible fields changed is
    ///   updated in place;
    /// - still-pending suggestions attributed to this resource but no
    ///   longer in it are withdrawn (deleted: they were never decided,
    ///   so no decision history goes with them).
    ///
    /// Confirmed and dismissed rows are never touched, and pending
    /// rows that cannot be attributed to any resource (fetched before
    /// v5, or from a `.ics` file) are never withdrawn — only rows this
    /// resource can prove are its own. Unattributed pending rows that
    /// match a fetched occurrence gain the attribution silently, so one
    /// full refresh after the upgrade converges them.
    pub fn reconcile_caldav_resource(
        &self,
        source_id: &str,
        href: &str,
        items: &[ExternalItem],
        now_ms: i64,
    ) -> StoreResult<ReconcileCounts> {
        use std::collections::HashSet;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let pending: Vec<PendingRow> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, external_uid, kind, title, started_at, ended_at, resource_href
                     FROM suggestions WHERE source_id = ?1 AND status = 'pending'",
                )
                .map_err(|e| sqlite(&self.path, e))?;
            let rows = stmt
                .query_map(params![source_id], |row| {
                    Ok(PendingRow {
                        id: row.get(0)?,
                        uid: row.get(1)?,
                        kind: row.get(2)?,
                        title: row.get(3)?,
                        started_at: row.get(4)?,
                        ended_at: row.get(5)?,
                        href: row.get(6)?,
                    })
                })
                .map_err(|e| sqlite(&self.path, e))?;
            rows.collect::<rusqlite::Result<_>>()
                .map_err(|e| sqlite(&self.path, e))?
        };
        let mut counts = ReconcileCounts::default();
        let mut seen: HashSet<(&str, i64)> = HashSet::new();
        for item in items {
            seen.insert((item.uid.as_str(), item.started_at));
            let kind = match item.kind {
                ExternalKind::Event => "event",
                ExternalKind::Task => "task",
            };
            match pending
                .iter()
                .find(|p| p.uid == item.uid && p.started_at == item.started_at)
            {
                Some(row) => {
                    let visible_changed = row.kind != kind
                        || row.title != item.title
                        || row.ended_at != item.ended_at;
                    if visible_changed || row.href.as_deref() != Some(href) {
                        tx.execute(
                            "UPDATE suggestions SET kind = ?2, title = ?3, ended_at = ?4,
                                 fetched_at = ?5, resource_href = ?6 WHERE id = ?1",
                            params![row.id, kind, item.title, item.ended_at, now_ms, href],
                        )
                        .map_err(|e| sqlite(&self.path, e))?;
                        if visible_changed {
                            counts.updated += 1;
                        }
                    }
                }
                None => {
                    let id = mint_id(&tx, "sg-").map_err(|e| sqlite(&self.path, e))?;
                    counts.inserted += tx
                        .execute(
                            "INSERT INTO suggestions(id, source_id, external_uid, kind, title,
                                 started_at, ended_at, fetched_at, resource_href)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                             ON CONFLICT(source_id, external_uid, started_at) DO NOTHING",
                            params![
                                id,
                                source_id,
                                item.uid,
                                kind,
                                item.title,
                                item.started_at,
                                item.ended_at,
                                now_ms,
                                href
                            ],
                        )
                        .map_err(|e| sqlite(&self.path, e))?;
                }
            }
        }
        for row in &pending {
            if row.href.as_deref() == Some(href)
                && !seen.contains(&(row.uid.as_str(), row.started_at))
            {
                tx.execute("DELETE FROM suggestions WHERE id = ?1", params![row.id])
                    .map_err(|e| sqlite(&self.path, e))?;
                counts.withdrawn += 1;
            }
        }
        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(counts)
    }

    /// Withdraw a remotely deleted resource: its still-pending
    /// suggestions go, and its validator row with them. Confirmed and
    /// dismissed rows stay — entries are tracked time now, and a
    /// dismissal must survive the event coming back. Returns how many
    /// pending suggestions were withdrawn.
    pub fn withdraw_caldav_resource(&self, source_id: &str, href: &str) -> StoreResult<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|e| sqlite(&self.path, e))?;
        let withdrawn = tx
            .execute(
                "DELETE FROM suggestions
                 WHERE source_id = ?1 AND resource_href = ?2 AND status = 'pending'",
                params![source_id, href],
            )
            .map_err(|e| sqlite(&self.path, e))?;
        tx.execute(
            "DELETE FROM caldav_resources WHERE source_id = ?1 AND href = ?2",
            params![source_id, href],
        )
        .map_err(|e| sqlite(&self.path, e))?;
        tx.commit().map_err(|e| sqlite(&self.path, e))?;
        Ok(withdrawn)
    }
}

/// A registered external calendar/task source (§19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSource {
    pub id: String,
    /// `ics_file` or `ics_url` (CalDAV kinds join later).
    pub kind: String,
    /// The file path or URL the service fetches, read-only.
    pub locator: String,
    /// The project suggestions from this source land on, if mapped.
    pub project_id: Option<String>,
    pub label: String,
    /// The OS credential store entry holding this source's secret
    /// (#149, schema v4): `Some` exactly when a credential was set.
    /// A reference only — the secret itself lives in the OS store,
    /// never in this database.
    pub cred_ref: Option<String>,
    /// The non-secret login name the credential authenticates as.
    pub cred_username: Option<String>,
}

/// A source's CalDAV sync state (#151): the sync token from the
/// last `sync-collection` REPORT, and/or the collection CTag from the
/// last `PROPFIND`, depending on what the server speaks. Either may be
/// absent; both absent (or no row at all) means "never synced".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaldavSyncState {
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
}

/// One known CalDAV resource with the validators its last fetch left
/// behind: the server ETag when it sends one, always a content hash.
/// The hash is the fallback comparator for servers without ETags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaldavResource {
    pub href: String,
    pub etag: Option<String>,
    pub content_hash: Option<String>,
}

/// What reconciling one resource did to its pending suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileCounts {
    pub inserted: usize,
    pub updated: usize,
    pub withdrawn: usize,
}

/// One pending suggestion row as the CalDAV reconcile reads it:
/// the identity, the visible fields, and the resource attribution.
struct PendingRow {
    id: String,
    uid: String,
    kind: String,
    title: String,
    started_at: i64,
    ended_at: i64,
    href: Option<String>,
}

/// A fetched event/task awaiting (or past) the user's decision (§19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub source_id: String,
    pub external_uid: String,
    /// `event` or `task`.
    pub kind: String,
    pub title: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// `pending`, `confirmed`, or `dismissed`.
    pub status: String,
    pub entry_id: Option<String>,
}

/// Sync configuration and watermarks persisted in `meta` (#173).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncConfig {
    pub folder: Option<String>,
    pub merged_through: i64,
    pub compacted_through: i64,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    pub needs_repair: bool,
}

/// A project row with full sync metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub project: Project,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// An entry row with full sync metadata and optional external UID carriage (#174).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    pub entry: Entry,
    pub external_source_id: Option<String>,
    pub external_uid: Option<String>,
    pub external_started_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// An external-source row with full sync metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSourceRow {
    pub source: ExternalSource,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// All syncable local store state read in one snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncRows {
    pub projects: Vec<ProjectRow>,
    pub entries: Vec<EntryRow>,
    pub sources: Vec<ExternalSourceRow>,
    pub tombstones: Vec<Tombstone>,
}

/// A local losing record recorded by LWW conflict resolution (#173).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConflictRow {
    pub id: String,
    pub kind: String,
    pub record_id: String,
    pub recorded_at: i64,
    pub loser_updated_at: i64,
    pub loser_device_id: String,
    pub winner_updated_at: i64,
    pub winner_device_id: String,
    pub loser_payload: String,
    pub restored_at: Option<i64>,
}

/// A tracked peer device seen in the sync folder (#173, #175).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPeerRow {
    pub device_id: String,
    pub last_published_at: i64,
    pub last_merged_through: i64,
    pub last_seen_at: i64,
    pub quarantined: bool,
    pub last_warning: Option<String>,
}

/// The payload restored into the store when a conflict loser is re-saved (#173).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoredRecord {
    Project(Project),
    Entry {
        entry: Entry,
        external_source_id: Option<String>,
        external_uid: Option<String>,
        external_started_at: Option<i64>,
    },
    Source {
        id: String,
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
    },
}

/// Transactional write plan produced by a sync merge round (#173, #175).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncMergeWrite {
    pub upsert_projects: Vec<ProjectRow>,
    pub delete_projects: Vec<String>,
    pub upsert_entries: Vec<EntryRow>,
    pub delete_entries: Vec<String>,
    pub upsert_sources: Vec<ExternalSourceRow>,
    pub delete_sources: Vec<String>,
    pub upsert_tombstones: Vec<Tombstone>,
    pub delete_tombstones: Vec<(String, String)>,
    pub new_conflicts: Vec<SyncConflictRow>,
    pub upsert_peers: Vec<SyncPeerRow>,
    pub delete_peers: Vec<String>,
    pub merged_through: Option<i64>,
    pub compacted_through: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<Option<String>>,
    pub needs_repair: Option<bool>,
}

fn delete_source_rows(tx: &Connection, id: &str) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM suggestions WHERE source_id = ?1 AND status = 'pending'",
        params![id],
    )?;
    tx.execute("DELETE FROM caldav_sync WHERE source_id = ?1", params![id])?;
    tx.execute(
        "DELETE FROM caldav_resources WHERE source_id = ?1",
        params![id],
    )?;
    Ok(())
}

fn mint_id(conn: &Connection, prefix: &str) -> rusqlite::Result<String> {
    let hex: String = conn.query_row("SELECT lower(hex(randomblob(8)))", [], |r| r.get(0))?;
    Ok(format!("{prefix}{hex}"))
}

fn source_code(source: EntrySource) -> i64 {
    match source {
        EntrySource::Manual => 0,
        EntrySource::QuickAdd => 1,
        EntrySource::External => 2,
    }
}

fn upsert_project(
    conn: &Connection,
    p: &Project,
    now_ms: i64,
    device_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO projects(id, name, colour, archived, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, colour = excluded.colour,
             archived = excluded.archived, updated_at = excluded.updated_at,
             device_id = excluded.device_id",
        params![
            p.id,
            p.name,
            p.colour.map(|c| c as i64),
            p.archived as i64,
            now_ms,
            device_id
        ],
    )?;
    Ok(())
}

fn upsert_entry(
    conn: &Connection,
    e: &Entry,
    now_ms: i64,
    device_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO entries(id, project_id, description, started_at, ended_at, source, note,
             created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET project_id = excluded.project_id,
             description = excluded.description, started_at = excluded.started_at,
             ended_at = excluded.ended_at, source = excluded.source, note = excluded.note,
             updated_at = excluded.updated_at, device_id = excluded.device_id",
        params![
            e.id,
            e.project_id,
            e.description,
            e.started_at,
            e.ended_at,
            source_code(e.source),
            e.note,
            now_ms,
            device_id
        ],
    )?;
    Ok(())
}

fn tombstone(
    conn: &Connection,
    id: &str,
    kind: &str,
    now_ms: i64,
    device_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tombstones(id, kind, deleted_at, device_id) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id, kind) DO UPDATE SET deleted_at = excluded.deleted_at,
             device_id = excluded.device_id",
        params![id, kind, now_ms, device_id],
    )?;
    Ok(())
}

/// The v1 schema plus its `user_version` bump, run inside one
/// transaction by the migration step. Tests build v2 databases from
/// these same statements, so the migration under test can only ever
/// run against the real historical shape.
const SCHEMA_V1: &str = "CREATE TABLE meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE projects (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,
                 name TEXT NOT NULL,
                 colour INTEGER,
                 archived INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL DEFAULT 0,
                 device_id TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE entries (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,
                 project_id TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '',
                 started_at INTEGER NOT NULL,
                 ended_at INTEGER NOT NULL,
                 source INTEGER NOT NULL DEFAULT 0,
                 note TEXT,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL DEFAULT 0,
                 device_id TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX idx_entries_started ON entries(started_at);
             CREATE INDEX idx_entries_project ON entries(project_id);
             CREATE TABLE tombstones (
                 id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 deleted_at INTEGER NOT NULL,
                 device_id TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (id, kind)
             );
             PRAGMA user_version = 1;";

/// The v2 addition (§19) plus its bump. See [`SCHEMA_V1`].
const SCHEMA_V2: &str = "CREATE TABLE external_sources (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,
                 kind TEXT NOT NULL,
                 locator TEXT NOT NULL,
                 project_id TEXT,
                 label TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL DEFAULT 0,
                 device_id TEXT NOT NULL DEFAULT '',
                 UNIQUE(kind, locator)
             );
             CREATE TABLE suggestions (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,
                 source_id TEXT NOT NULL,
                 external_uid TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 started_at INTEGER NOT NULL,
                 ended_at INTEGER NOT NULL,
                 status TEXT NOT NULL DEFAULT 'pending',
                 entry_id TEXT,
                 fetched_at INTEGER NOT NULL DEFAULT 0,
                 decided_at INTEGER,
                 UNIQUE(source_id, external_uid, started_at)
             );
             CREATE INDEX idx_suggestions_status ON suggestions(status, started_at);
             PRAGMA user_version = 2;";

/// The v4 addition (#149) plus its bump: each external source gains
/// an OS credential-store reference and a non-secret login name. Pure
/// additions — existing rows read back with both unset. See [`SCHEMA_V1`].
const SCHEMA_V4: &str = "ALTER TABLE external_sources ADD COLUMN cred_ref TEXT;
             ALTER TABLE external_sources ADD COLUMN cred_username TEXT;
             PRAGMA user_version = 4;";

/// The v5 addition (#151) plus its bump: CalDAV incremental sync
/// state. `caldav_sync` holds one row per synced source (sync token
/// and/or CTag), `caldav_resources` holds one row per fetched
/// resource (ETag and/or content hash), and every suggestion gains
/// the resource it was fetched from — `NULL` for `.ics` sources and
/// for rows fetched before this migration, which is exactly the
/// "cannot attribute, never withdraw" state. Pure additions, like v4.
/// See [`SCHEMA_V1`].
const SCHEMA_V5: &str = "CREATE TABLE caldav_sync (
                 source_id TEXT PRIMARY KEY,
                 sync_token TEXT,
                 ctag TEXT,
                 updated_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE caldav_resources (
                 source_id TEXT NOT NULL,
                 href TEXT NOT NULL,
                 etag TEXT,
                 content_hash TEXT,
                 seen_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (source_id, href)
             );
             CREATE INDEX idx_caldav_resources_source ON caldav_resources(source_id);
             ALTER TABLE suggestions ADD COLUMN resource_href TEXT;
             PRAGMA user_version = 5;";

/// The v7 addition (#173) plus its bump: local sync conflict journal
/// (never synced, never auto-deleted) and tracked sync peers. Pure
/// additions — no existing row moves. See [`SCHEMA_V1`].
const SCHEMA_V7: &str = "CREATE TABLE sync_conflicts (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 record_id TEXT NOT NULL,
                 recorded_at INTEGER NOT NULL,
                 loser_updated_at INTEGER NOT NULL,
                 loser_device_id TEXT NOT NULL,
                 winner_updated_at INTEGER NOT NULL,
                 winner_device_id TEXT NOT NULL,
                 loser_payload TEXT NOT NULL,
                 restored_at INTEGER,
                 UNIQUE(kind, record_id, loser_updated_at, loser_device_id)
             );
             CREATE TABLE sync_peers (
                 device_id TEXT PRIMARY KEY,
                 last_published_at INTEGER NOT NULL,
                 last_merged_through INTEGER NOT NULL,
                 last_seen_at INTEGER NOT NULL,
                 quarantined INTEGER NOT NULL DEFAULT 0,
                 last_warning TEXT
             );
             PRAGMA user_version = 7;";

/// Numbered schema migrations, applied in order on open. Each step runs
/// in its own transaction with the `user_version` bump inside it, so a
/// crash mid-migration leaves the previous version whole.
fn migrate(conn: &mut Connection, path: &Path) -> StoreResult<()> {
    migrate_with(conn, path, &mut || Ok(uuid::Uuid::new_v4().to_string()))
}

/// [`migrate`] with the UUID generator injected, so the rollback test
/// can fail the remint mid-step and prove the database stays v2.
fn migrate_with(
    conn: &mut Connection,
    path: &Path,
    mint: &mut dyn FnMut() -> StoreResult<String>,
) -> StoreResult<()> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| sqlite(path, e))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::Invalid {
            path: path.display().to_string(),
            reason: format!(
                "written by a newer TimeTrack (schema v{version}, this build reads v{SCHEMA_VERSION})"
            ),
        });
    }
    if version < 1 {
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(SCHEMA_V1).map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 1;
    }
    if version < 2 {
        // §19: external sources and their suggestions. Pure additions —
        // no existing row moves, so a v1 database gains two empty tables.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(SCHEMA_V2).map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 2;
    }
    if version < 3 {
        // ADR 0018: remint every sequential id as a UUID, rewriting all
        // references. One transaction: a generator failure aborts the
        // whole step and the database stays v2, untouched.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        remint_v2_ids(&tx, path, mint)?;
        tx.execute_batch("PRAGMA user_version = 3;")
            .map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 3;
    }
    if version < 4 {
        // #149: credential references on external sources. Pure
        // additions — no existing row moves.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(SCHEMA_V4).map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 4;
    }
    if version < 5 {
        // #151: CalDAV sync state and resource attribution. Pure
        // additions — no existing row moves.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(SCHEMA_V5).map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 5;
    }
    if version < 6 {
        // #172 (ADR 0020): remint `src-{rand}` source ids to UUIDv4,
        // preserving `cred_ref` as a per-device OS keyring pointer.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        remint_v5_source_ids(&tx, path, mint)?;
        tx.execute_batch("PRAGMA user_version = 6;")
            .map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 6;
    }
    if version < 7 {
        // #173 (ADR 0020): local conflict journal and tracked peer watermarks.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(SCHEMA_V7).map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
    }
    Ok(())
}

/// Remint every non-UUID entry/project id a v2 database holds (ADR 0018).
///
/// Projects and entries gain fresh ids from `mint`, and every reference
/// follows the mapping: entry→project links, source→project links, and
/// suggestion→entry links. Tombstones remint by kind through the same
/// mapping — or gain a fresh id when their record is already gone and
/// there is no live row to map from. Ids already in UUID shape are left
/// untouched, so the step converges mixed states without rewriting
/// identities that need no rewriting.
fn remint_v2_ids(
    conn: &Connection,
    path: &Path,
    mint: &mut dyn FnMut() -> StoreResult<String>,
) -> StoreResult<()> {
    use std::collections::HashMap;
    let mut projects: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id FROM projects ORDER BY seq")
            .map_err(|e| sqlite(path, e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite(path, e))?;
        for row in rows {
            let old = row.map_err(|e| sqlite(path, e))?;
            if !is_uuid(&old) {
                projects.insert(old, mint()?);
            }
        }
    }
    let mut entries: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id FROM entries ORDER BY seq")
            .map_err(|e| sqlite(path, e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite(path, e))?;
        for row in rows {
            let old = row.map_err(|e| sqlite(path, e))?;
            if !is_uuid(&old) {
                entries.insert(old, mint()?);
            }
        }
    }
    for (old, new) in &projects {
        conn.execute(
            "UPDATE projects SET id = ?1 WHERE id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    for (old, new) in &entries {
        conn.execute(
            "UPDATE entries SET id = ?1 WHERE id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    for (old, new) in &projects {
        conn.execute(
            "UPDATE entries SET project_id = ?1 WHERE project_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
        conn.execute(
            "UPDATE external_sources SET project_id = ?1 WHERE project_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    for (old, new) in &entries {
        conn.execute(
            "UPDATE suggestions SET entry_id = ?1 WHERE entry_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    let stones: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, kind FROM tombstones")
            .map_err(|e| sqlite(path, e))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| sqlite(path, e))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|e| sqlite(path, e))?
    };
    for (old, kind) in &stones {
        if is_uuid(old) {
            continue;
        }
        let mapped: Option<&String> = match kind.as_str() {
            "entry" => entries.get(old),
            "project" => projects.get(old),
            _ => None,
        };
        let new = match mapped {
            Some(id) => id.clone(),
            None => mint()?,
        };
        conn.execute(
            "UPDATE tombstones SET id = ?1 WHERE id = ?2 AND kind = ?3",
            params![new, old, kind],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    Ok(())
}

/// Remint every non-UUID external-source id a v5 database holds (#172).
///
/// External sources and any orphaned `suggestions` / `caldav_*` source
/// references gain fresh UUIDs from `mint`, and `suggestions.source_id`,
/// `caldav_sync.source_id`, `caldav_resources.source_id`, plus any
/// `kind='source'` tombstones follow. `cred_ref` is deliberately left
/// untouched because it is a per-device OS credential store pointer.
fn remint_v5_source_ids(
    conn: &Connection,
    path: &Path,
    mint: &mut dyn FnMut() -> StoreResult<String>,
) -> StoreResult<()> {
    use std::collections::HashMap;
    let mut sources: HashMap<String, String> = HashMap::new();
    for query in [
        "SELECT id FROM external_sources ORDER BY seq",
        "SELECT DISTINCT source_id FROM suggestions ORDER BY source_id",
        "SELECT DISTINCT source_id FROM caldav_sync ORDER BY source_id",
        "SELECT DISTINCT source_id FROM caldav_resources ORDER BY source_id",
    ] {
        let mut stmt = conn.prepare(query).map_err(|e| sqlite(path, e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite(path, e))?;
        for row in rows {
            let old = row.map_err(|e| sqlite(path, e))?;
            if !is_uuid(&old) && !sources.contains_key(&old) {
                sources.insert(old, mint()?);
            }
        }
    }
    for (old, new) in &sources {
        conn.execute(
            "UPDATE external_sources SET id = ?1 WHERE id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
        conn.execute(
            "UPDATE suggestions SET source_id = ?1 WHERE source_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
        conn.execute(
            "UPDATE caldav_sync SET source_id = ?1 WHERE source_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
        conn.execute(
            "UPDATE caldav_resources SET source_id = ?1 WHERE source_id = ?2",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    let stones: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM tombstones WHERE kind = 'source' ORDER BY deleted_at, id")
            .map_err(|e| sqlite(path, e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite(path, e))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|e| sqlite(path, e))?
    };
    for old in &stones {
        if is_uuid(old) {
            continue;
        }
        let new = match sources.get(old) {
            Some(id) => id.clone(),
            None => mint()?,
        };
        conn.execute(
            "UPDATE tombstones SET id = ?1 WHERE id = ?2 AND kind = 'source'",
            params![new, old],
        )
        .map_err(|e| sqlite(path, e))?;
    }
    Ok(())
}

/// This installation's device id, minted once with SQLite's own
/// randomness (no OS or clock dependency) and kept in `meta`.
fn ensure_device_id(conn: &Connection, path: &Path) -> StoreResult<String> {
    let existing: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| {
            r.get(0)
        })
        .ok();
    if let Some(id) = existing.filter(|s| !s.is_empty()) {
        return Ok(id);
    }
    let id: String = conn
        .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))
        .map_err(|e| sqlite(path, e))?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('device_id', ?1)
         ON CONFLICT(key) DO NOTHING",
        params![id],
    )
    .map_err(|e| sqlite(path, e))?;
    // Re-read: a racing opener may have minted first; the stored value wins.
    conn.query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| {
        r.get(0)
    })
    .map_err(|e| sqlite(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "timetrack-store-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Store {
        Store {
            version: STORE_VERSION,
            projects: vec![
                Project {
                    id: "p1".into(),
                    name: "Work".into(),
                    colour: Some(0x2ea043),
                    archived: false,
                },
                Project {
                    id: "p2".into(),
                    name: "Home".into(),
                    colour: None,
                    archived: true,
                },
            ],
            entries: vec![
                Entry {
                    id: "e1".into(),
                    project_id: "p1".into(),
                    description: "review".into(),
                    started_at: 1_000,
                    ended_at: 61_000,
                    source: EntrySource::Manual,
                    note: Some("n".into()),
                },
                Entry {
                    id: "e2".into(),
                    project_id: "p2".into(),
                    description: String::new(),
                    started_at: 120_000,
                    ended_at: 150_000,
                    source: EntrySource::QuickAdd,
                    note: None,
                },
            ],
        }
    }

    #[test]
    fn save_all_then_load_round_trips_in_order() {
        let path = dir("rt").join("store.db");
        let db = SqliteStore::open_file(&path).unwrap();
        db.save_all(&sample(), 999).unwrap();
        assert_eq!(db.load().unwrap(), sample());
        // A reopened database reads the same store and the same device.
        let again = SqliteStore::open_file(&path).unwrap();
        assert_eq!(again.load().unwrap(), sample());
        assert_eq!(again.device_id(), db.device_id());
        assert_eq!(db.device_id().len(), 32);
    }

    #[test]
    fn apply_upserts_and_tombstones_removals() {
        let db = SqliteStore::open_file(&dir("apply").join("store.db")).unwrap();
        let before = sample();
        db.save_all(&before, 1_000).unwrap();
        let mut after = before.clone();
        after.entries[0].description = "edited".into();
        after.entries.remove(1);
        let diff = Journal::between(&before, &after);
        db.apply(&diff, 2_000).unwrap();
        assert_eq!(db.load().unwrap(), after);
        let stones = db.tombstones().unwrap();
        assert_eq!(stones.len(), 1);
        assert_eq!(stones[0].id, "e2");
        assert_eq!(stones[0].kind, "entry");
        assert_eq!(stones[0].deleted_at, 2_000);
        assert_eq!(stones[0].device_id, db.device_id());
        // An empty diff is a no-op, like the journal's was.
        db.apply(&Journal::default(), 3_000).unwrap();
        assert_eq!(db.load().unwrap(), after);
    }

    #[test]
    fn open_at_imports_json_once_and_leaves_it_untouched() {
        let d = dir("import");
        let json_path = d.join("store.json");
        let json = JsonStore::new(json_path.clone());
        json.save(&sample()).unwrap();
        // A journal-only change on top of the checkpoint, as a live v1
        // store would hold: the importer must replay it.
        let mut after = sample();
        after.entries[0].description = "journaled".into();
        json.append(&Journal::between(&sample(), &after)).unwrap();
        let checkpoint = std::fs::read(&json_path).unwrap();
        let journal = std::fs::read(json.journal_path()).unwrap();

        let db = SqliteStore::open_at(&json_path).unwrap();
        // ADR 0018: the import remints sequential ids, so compare by
        // content rather than by id — same projects, same entries on the
        // same projects, journal replayed.
        let imported = db.load().unwrap();
        assert_eq!(imported.projects.len(), after.projects.len());
        assert_eq!(imported.entries.len(), after.entries.len());
        assert_eq!(
            imported.entries[0].description, "journaled",
            "checkpoint + journal imported"
        );
        for p in &imported.projects {
            assert!(is_uuid(&p.id), "project id reminted: {}", p.id);
        }
        for e in &imported.entries {
            assert!(is_uuid(&e.id), "entry id reminted: {}", e.id);
            assert!(
                is_uuid(&e.project_id),
                "entry link reminted: {}",
                e.project_id
            );
        }
        let work = imported.projects.iter().find(|p| p.name == "Work").unwrap();
        assert_eq!(imported.entries[0].project_id, work.id);
        assert_eq!(
            std::fs::read(&json_path).unwrap(),
            checkpoint,
            "checkpoint untouched"
        );
        assert_eq!(
            std::fs::read(json.journal_path()).unwrap(),
            journal,
            "journal untouched"
        );
        // A second open does not re-import over newer database contents.
        let mut newer = imported.clone();
        newer.entries[0].description = "in db".into();
        db.apply(&Journal::between(&imported, &newer), 6_000)
            .unwrap();
        let reopened = SqliteStore::open_at(&json_path).unwrap();
        assert_eq!(reopened.load().unwrap(), newer);
    }

    #[test]
    fn open_at_without_json_starts_empty_and_corrupt_json_refuses() {
        let d = dir("fresh");
        let db = SqliteStore::open_at(&d.join("store.json")).unwrap();
        assert_eq!(db.load().unwrap().entries.len(), 0);

        let d = dir("corrupt");
        let json_path = d.join("store.json");
        std::fs::write(&json_path, "{ not json").unwrap();
        let err = SqliteStore::open_at(&json_path).unwrap_err();
        assert!(
            err.to_string().contains("could not be read"),
            "corrupt legacy store keeps its refusal: {err}"
        );
    }

    #[test]
    fn sources_and_suggestions_dedupe_and_decide_once() {
        let db = SqliteStore::open_file(&dir("sugg").join("store.db")).unwrap();
        let src = db
            .add_source("ics_file", "/tmp/cal.ics", Some("p1"), "Cal", 100)
            .unwrap();
        let again = db
            .add_source("ics_file", "/tmp/cal.ics", None, "", 200)
            .unwrap();
        assert_eq!(src.id, again.id, "same kind+locator is one source");
        let item = timetrack_core::ExternalItem {
            uid: "ev1".into(),
            kind: timetrack_core::ExternalKind::Event,
            title: "Review".into(),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        assert_eq!(
            db.upsert_suggestions(&src.id, std::slice::from_ref(&item), 1_000)
                .unwrap(),
            1
        );
        assert_eq!(
            db.upsert_suggestions(&src.id, &[item], 2_000).unwrap(),
            0,
            "a refetch inserts nothing twice"
        );
        let pending = db.suggestions(true).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            db.decide_suggestion(&pending[0].id, "dismissed", None, 3_000)
                .unwrap()
        );
        assert!(
            !db.decide_suggestion(&pending[0].id, "confirmed", Some("e9"), 4_000)
                .unwrap(),
            "deciding twice is a no-op"
        );
        assert_eq!(db.suggestions(true).unwrap().len(), 0);
        assert_eq!(db.suggestions(false).unwrap()[0].status, "dismissed");
        let v: i64 = db
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_schema_is_refused_not_reset() {
        let d = dir("newer");
        let path = d.join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = SqliteStore::open_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("newer TimeTrack"),
            "too-new database is named: {err}"
        );
    }

    #[test]
    fn is_uuid_accepts_only_hyphenated_hex() {
        assert!(is_uuid("aaaaaaaa-0000-4000-8000-000000000001"));
        assert!(is_uuid("AAAAAAAA-0000-4000-8000-000000000001"));
        for bad in [
            "",
            "e1",
            "p1",
            "aaaaaaaa-0000-4000-8000-00000000001",
            "aaaaaaaa-0000-4000-8000-0000000000012",
            "aaaaaaa-0000-4000-8000-000000000001",
            "aaaaaaaa_0000-4000-8000-000000000001",
            "gggggggg-0000-4000-8000-000000000001",
            "aaaaaaaa-0000-4000-8000-00000000000g",
        ] {
            assert!(!is_uuid(bad), "{bad:?} is not a UUID");
        }
    }

    /// A v2 database holding sequential ids everywhere they can hide:
    /// projects, entries, a source mapping, a confirmed suggestion's
    /// entry link, and tombstones for already-deleted records. Built
    /// from the real historical schema statements, never a copy.
    fn v2_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute(
            "INSERT INTO projects(id, name, colour, archived, created_at, updated_at, device_id)
             VALUES ('p1', 'Work', 255, 0, 1, 2, 'dev'),
                    ('p2', 'Home', NULL, 1, 1, 2, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries(id, project_id, description, started_at, ended_at, source, note,
                 created_at, updated_at, device_id)
             VALUES ('e1', 'p1', 'review', 1000, 61000, 0, 'n', 1, 2, 'dev'),
                    ('e2', 'p2', '', 120000, 150000, 1, NULL, 1, 2, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tombstones(id, kind, deleted_at, device_id)
             VALUES ('e9', 'entry', 3000, 'dev'), ('p9', 'project', 3000, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_sources(id, kind, locator, project_id, label,
                 created_at, updated_at, device_id)
             VALUES ('src-1', 'ics_file', '/tmp/cal.ics', 'p1', 'Cal', 1, 2, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO suggestions(id, source_id, external_uid, kind, title, started_at,
                 ended_at, status, entry_id, fetched_at, decided_at)
             VALUES ('sg-1', 'src-1', 'ev1', 'event', 'Review', 1000, 61000,
                 'confirmed', 'e1', 1000, 2000),
                    ('sg-2', 'src-1', 'ev2', 'task', 'Pending', 2000, 26000,
                 'pending', NULL, 1000, NULL)",
            [],
        )
        .unwrap();
    }

    /// Every row of a database, so the rollback test can prove a failed
    /// migration touched nothing.
    fn dump_all(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut out = Vec::new();
        for table in [
            "meta",
            "projects",
            "entries",
            "tombstones",
            "external_sources",
            "suggestions",
        ] {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let cols = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    (0..cols)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap();
            for row in rows {
                out.push(format!("{table}: {:?}", row.unwrap()));
            }
        }
        out
    }

    #[test]
    fn migration_v2_to_v3_remints_ids_and_keeps_everything_else() {
        let path = dir("remint").join("store.db");
        v2_database(&path);
        let db = SqliteStore::open_file(&path).unwrap();
        let store = db.load().unwrap();
        assert_eq!(store.projects.len(), 2);
        assert_eq!(store.entries.len(), 2);
        for p in &store.projects {
            assert!(is_uuid(&p.id), "project reminted: {}", p.id);
        }
        for e in &store.entries {
            assert!(is_uuid(&e.id), "entry reminted: {}", e.id);
            assert!(is_uuid(&e.project_id), "link reminted: {}", e.project_id);
        }
        let work = store.projects.iter().find(|p| p.name == "Work").unwrap();
        let home = store.projects.iter().find(|p| p.name == "Home").unwrap();
        assert_eq!(work.colour, Some(255));
        assert!(!work.archived);
        assert!(home.archived);
        let review = store
            .entries
            .iter()
            .find(|e| e.description == "review")
            .unwrap();
        assert_eq!(review.project_id, work.id);
        assert_eq!((review.started_at, review.ended_at), (1_000, 61_000));
        assert_eq!(review.source, EntrySource::Manual);
        assert_eq!(review.note.as_deref(), Some("n"));
        let quick = store
            .entries
            .iter()
            .find(|e| e.description.is_empty())
            .unwrap();
        assert_eq!(quick.project_id, home.id);
        assert_eq!(quick.source, EntrySource::QuickAdd);
        assert_eq!(store.total_ms(), 60_000 + 30_000);
        // The source mapping and the confirmed suggestion's link followed;
        // source ids reminted to UUIDs by v5→v6 (#172).
        let sources = db.sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert!(
            is_uuid(&sources[0].id),
            "source id reminted: {}",
            sources[0].id
        );
        assert_eq!(sources[0].project_id.as_deref(), Some(work.id.as_str()));
        let suggestions = db.suggestions(false).unwrap();
        assert_eq!(suggestions.len(), 2);
        let confirmed = suggestions.iter().find(|s| s.id == "sg-1").unwrap();
        assert_eq!(confirmed.source_id, sources[0].id);
        assert_eq!(confirmed.entry_id.as_deref(), Some(review.id.as_str()));
        let pending = suggestions.iter().find(|s| s.id == "sg-2").unwrap();
        assert_eq!(pending.source_id, sources[0].id);
        assert_eq!(pending.entry_id, None);
        // Tombstones for deleted records gained fresh ids; kinds and
        // timestamps preserved.
        let stones = db.tombstones().unwrap();
        assert_eq!(stones.len(), 2);
        for stone in &stones {
            assert!(is_uuid(&stone.id), "tombstone reminted: {}", stone.id);
            assert_eq!(stone.deleted_at, 3_000);
            assert_eq!(stone.device_id, "dev");
        }
        assert!(stones.iter().any(|s| s.kind == "entry"));
        assert!(stones.iter().any(|s| s.kind == "project"));
        // Schema bumped, record order preserved.
        let v: i64 = db
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(store.projects[0].name, "Work");
        assert_eq!(store.projects[1].name, "Home");
    }

    #[test]
    fn a_failed_remint_rolls_back_to_an_untouched_v2() {
        let path = dir("remint-rollback").join("store.db");
        v2_database(&path);
        let before = dump_all(&path);
        // Fail while minting tombstone ids: the project and entry
        // UPDATEs already ran inside the migration transaction, so only
        // a real rollback leaves the rows byte-identical.
        let mut calls = 0;
        let mut failing = || -> StoreResult<String> {
            calls += 1;
            if calls > 4 {
                return Err(StoreError::Invalid {
                    path: "test".into(),
                    reason: "boom".into(),
                });
            }
            Ok(format!("00000000-0000-4000-8000-{calls:012}"))
        };
        let mut conn = Connection::open(&path).unwrap();
        let err = migrate_with(&mut conn, &path, &mut failing).unwrap_err();
        assert!(
            err.to_string().contains("boom"),
            "the generator failure surfaces: {err}"
        );
        drop(conn);
        let v: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2, "a failed migration keeps the old version");
        assert_eq!(dump_all(&path), before, "a failed migration touches no row");
        // And the database is not wedged: the next open migrates cleanly.
        let db = SqliteStore::open_file(&path).unwrap();
        assert!(
            db.load().unwrap().projects.iter().all(|p| is_uuid(&p.id)),
            "a later open still remints"
        );
    }

    #[test]
    fn remint_store_ids_leaves_uuids_alone() {
        let keep = "aaaaaaaa-0000-4000-8000-000000000001".to_string();
        let mut store = Store {
            version: STORE_VERSION,
            projects: vec![
                Project {
                    id: keep.clone(),
                    name: "Kept".into(),
                    colour: None,
                    archived: false,
                },
                Project {
                    id: "p2".into(),
                    name: "Reminted".into(),
                    colour: None,
                    archived: false,
                },
            ],
            entries: vec![Entry {
                id: "e1".into(),
                project_id: "p2".into(),
                description: "x".into(),
                started_at: 0,
                ended_at: 1,
                source: EntrySource::Manual,
                note: None,
            }],
        };
        let mut n = 0;
        remint_store_ids(&mut store, &mut || {
            n += 1;
            Ok(format!("00000000-0000-4000-8000-{n:012}"))
        })
        .unwrap();
        assert_eq!(store.projects[0].id, keep, "a UUID id is never rewritten");
        assert!(is_uuid(&store.projects[1].id));
        assert!(is_uuid(&store.entries[0].id));
        assert_eq!(store.entries[0].project_id, store.projects[1].id);
        assert_eq!(n, 2, "only the two sequential ids mint");
    }

    #[test]
    fn migration_v3_to_v4_adds_credential_columns_and_keeps_rows() {
        // A v3 database: the v2 schema statements plus the version bump
        // (the v2→v3 remint changes ids, not the schema, so a bumped v2
        // shape is schema-v3 for this migration's purposes).
        let path = dir("cred-migrate").join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute(
                "INSERT INTO external_sources(id, kind, locator, project_id, label,
                     created_at, updated_at, device_id)
                 VALUES ('src-1', 'caldav', 'https://cal.test/u/', NULL, 'Cal', 1, 2, 'dev')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let db = SqliteStore::open_file(&path).unwrap();
        let v: i64 = db
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let sources = db.sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].locator, "https://cal.test/u/");
        assert_eq!(sources[0].cred_ref, None, "old rows gain no credential");
        assert_eq!(sources[0].cred_username, None);
    }

    #[test]
    fn credential_refs_round_trip_and_remove_source_clears_pending_only() {
        let db = SqliteStore::open_file(&dir("cred-refs").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        assert_eq!(src.cred_ref, None);
        assert!(
            db.set_source_credential_ref(&src.id, "caldav/abc", "alice")
                .unwrap(),
            "setting on a known source"
        );
        assert!(
            !db.set_source_credential_ref("src-nope", "r", "u").unwrap(),
            "setting on an unknown source refuses"
        );
        let got = db.source(&src.id).unwrap().unwrap();
        assert_eq!(got.cred_ref.as_deref(), Some("caldav/abc"));
        assert_eq!(got.cred_username.as_deref(), Some("alice"));
        assert!(db.clear_source_credential_ref(&src.id).unwrap());
        assert!(
            !db.clear_source_credential_ref("src-nope").unwrap(),
            "clearing an unknown source refuses"
        );
        let cleared = db.source(&src.id).unwrap().unwrap();
        assert_eq!(cleared.cred_ref, None);
        assert_eq!(cleared.cred_username, None);

        // Removing the source takes its still-pending suggestions
        // with it (#153) — but decided rows stay as history.
        let item = |uid: &str| timetrack_core::ExternalItem {
            uid: uid.into(),
            kind: timetrack_core::ExternalKind::Event,
            title: format!("Event {uid}"),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        db.upsert_suggestions(&src.id, &[item("ev1"), item("ev2")], 1_000)
            .unwrap();
        let pending = db.suggestions(true).unwrap();
        assert_eq!(pending.len(), 2);
        db.decide_suggestion(&pending[0].id, "dismissed", None, 2_000)
            .unwrap();
        assert!(db.remove_source(&src.id, 3_000).unwrap());
        assert!(
            !db.remove_source(&src.id, 4_000).unwrap(),
            "twice is a no-op"
        );
        assert!(db.source(&src.id).unwrap().is_none());
        assert!(db.suggestions(true).unwrap().is_empty());
        let decided = db.suggestions(false).unwrap();
        assert_eq!(decided.len(), 1, "the decided row survives the removal");
        assert_eq!(decided[0].status, "dismissed");
    }

    #[test]
    fn update_source_rewrites_fields_and_leaves_suggestions_alone() {
        let db = SqliteStore::open_file(&dir("src-update").join("store.db")).unwrap();
        let src = db
            .add_source("ics_file", "/tmp/cal.ics", Some("p1"), "Cal", 100)
            .unwrap();
        let item = timetrack_core::ExternalItem {
            uid: "ev1".into(),
            kind: timetrack_core::ExternalKind::Event,
            title: "Review".into(),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        db.upsert_suggestions(&src.id, std::slice::from_ref(&item), 1_000)
            .unwrap();
        let pending = db.suggestions(true).unwrap();
        db.decide_suggestion(&pending[0].id, "dismissed", None, 2_000)
            .unwrap();
        db.upsert_suggestions(
            &src.id,
            &[timetrack_core::ExternalItem {
                uid: "ev2".into(),
                ..item.clone()
            }],
            3_000,
        )
        .unwrap();

        let updated = db
            .update_source(
                &src.id,
                "ics_url",
                "https://cal.test/cal.ics",
                None,
                "Remote",
                4_000,
            )
            .unwrap()
            .expect("a known source updates");
        assert_eq!(updated.id, src.id, "the identity never moves");
        assert_eq!(updated.kind, "ics_url");
        assert_eq!(updated.locator, "https://cal.test/cal.ics");
        assert_eq!(updated.project_id, None, "the mapping clears");
        assert_eq!(updated.label, "Remote");
        assert_eq!(
            db.source(&src.id).unwrap().unwrap(),
            updated,
            "the row reads back as written"
        );
        // Pending and decided suggestions alike survive the edit.
        assert_eq!(db.suggestions(true).unwrap().len(), 1);
        assert_eq!(db.suggestions(false).unwrap().len(), 2);
        assert!(
            db.update_source("src-nope", "ics_file", "/x", None, "X", 5_000)
                .unwrap()
                .is_none(),
            "an unknown source updates to nothing"
        );
    }

    #[test]
    fn update_source_purges_sync_state_only_when_identity_moves() {
        let db = SqliteStore::open_file(&dir("src-update-sync").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        db.set_caldav_sync_state(&src.id, Some("tok-1"), Some("ctag-1"), 200)
            .unwrap();
        db.upsert_caldav_resource(&src.id, "/cal/a.ics", Some("\"v1\""), Some("hash-1"), 200)
            .unwrap();

        // A label-only edit keeps the sync state: the collection did
        // not move, so the token still describes it.
        db.update_source(
            &src.id,
            "caldav",
            "https://cal.test/u/",
            None,
            "Renamed",
            300,
        )
        .unwrap()
        .unwrap();
        let state = db.caldav_sync_state(&src.id).unwrap().unwrap();
        assert_eq!(state.sync_token.as_deref(), Some("tok-1"));
        assert_eq!(db.caldav_resources(&src.id).unwrap().len(), 1);

        // A moved locator invalidates both; the next refresh starts whole.
        db.update_source(
            &src.id,
            "caldav",
            "https://cal.test/v/",
            None,
            "Renamed",
            400,
        )
        .unwrap()
        .unwrap();
        assert_eq!(db.caldav_sync_state(&src.id).unwrap(), None);
        assert!(db.caldav_resources(&src.id).unwrap().is_empty());
    }

    #[test]
    fn migration_v4_to_v5_adds_sync_tables_and_keeps_rows() {
        // A v4 database: the historical statements plus the bump, with
        // a source and a suggestion already in it.
        let path = dir("sync-migrate").join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V4).unwrap();
            conn.execute(
                "INSERT INTO external_sources(id, kind, locator, project_id, label,
                     created_at, updated_at, device_id)
                 VALUES ('src-1', 'caldav', 'https://cal.test/u/', NULL, 'Cal', 1, 2, 'dev')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO suggestions(id, source_id, external_uid, kind, title,
                     started_at, ended_at, fetched_at)
                 VALUES ('sg-1', 'src-1', 'ev1', 'event', 'Review', 1000, 61000, 3)",
                [],
            )
            .unwrap();
            let v: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 4);
        }
        let db = SqliteStore::open_file(&path).unwrap();
        let v: i64 = db
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Old rows survive, with no attribution — the state reconcile
        // treats as "cannot attribute, never withdraw".
        let sources = db.sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert!(is_uuid(&sources[0].id));
        let pending = db.suggestions(true).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source_id, sources[0].id);
        assert_eq!(pending[0].title, "Review");
        let href: Option<String> = db
            .lock()
            .query_row(
                "SELECT resource_href FROM suggestions WHERE id = 'sg-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(href, None);
        // And the new tables answer under the reminted source UUID.
        assert_eq!(db.caldav_sync_state(&sources[0].id).unwrap(), None);
        assert!(db.caldav_resources(&sources[0].id).unwrap().is_empty());
    }

    #[test]
    fn caldav_sync_state_and_resources_round_trip() {
        let db = SqliteStore::open_file(&dir("sync-rt").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        assert_eq!(db.caldav_sync_state(&src.id).unwrap(), None);
        db.set_caldav_sync_state(&src.id, Some("tok-1"), Some("ctag-1"), 200)
            .unwrap();
        assert_eq!(
            db.caldav_sync_state(&src.id).unwrap(),
            Some(CaldavSyncState {
                sync_token: Some("tok-1".into()),
                ctag: Some("ctag-1".into()),
            })
        );
        // Re-recording replaces both halves, and clears what is gone
        // (a PROPFIND resync after a dead token keeps no token).
        db.set_caldav_sync_state(&src.id, None, Some("ctag-2"), 300)
            .unwrap();
        assert_eq!(
            db.caldav_sync_state(&src.id).unwrap(),
            Some(CaldavSyncState {
                sync_token: None,
                ctag: Some("ctag-2".into()),
            })
        );
        db.upsert_caldav_resource(
            &src.id,
            "https://cal.test/u/a.ics",
            Some("\"1\""),
            Some("h1"),
            200,
        )
        .unwrap();
        db.upsert_caldav_resource(&src.id, "https://cal.test/u/b.ics", None, Some("h2"), 200)
            .unwrap();
        assert_eq!(
            db.caldav_resources(&src.id).unwrap(),
            [
                CaldavResource {
                    href: "https://cal.test/u/a.ics".into(),
                    etag: Some("\"1\"".into()),
                    content_hash: Some("h1".into()),
                },
                CaldavResource {
                    href: "https://cal.test/u/b.ics".into(),
                    etag: None,
                    content_hash: Some("h2".into()),
                },
            ]
        );
        // Removing the source takes the sync state with it.
        assert!(db.remove_source(&src.id, 400).unwrap());
        assert_eq!(db.caldav_sync_state(&src.id).unwrap(), None);
        assert!(db.caldav_resources(&src.id).unwrap().is_empty());
    }

    #[test]
    fn reconcile_inserts_updates_and_withdraws_only_its_own() {
        use timetrack_core::{ExternalItem, ExternalKind};
        let db = SqliteStore::open_file(&dir("reconcile").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        let item = |uid: &str, title: &str, start: i64| ExternalItem {
            uid: uid.into(),
            kind: ExternalKind::Event,
            title: title.into(),
            started_at: start,
            ended_at: start + 3_600_000,
            recurring: false,
        };
        // A first fetch attributes two occurrences to the resource.
        let counts = db
            .reconcile_caldav_resource(
                &src.id,
                "https://cal.test/u/a.ics",
                &[item("ev1", "Standup", 1_000), item("ev2", "Review", 2_000)],
                100,
            )
            .unwrap();
        assert_eq!(
            counts,
            ReconcileCounts {
                inserted: 2,
                updated: 0,
                withdrawn: 0,
            }
        );
        // A second resource's occurrence, plus an unattributed legacy
        // row (a `.ics` upsert, which sets no resource).
        db.reconcile_caldav_resource(
            &src.id,
            "https://cal.test/u/b.ics",
            &[item("ev3", "Other", 3_000)],
            100,
        )
        .unwrap();
        db.upsert_suggestions(&src.id, &[item("ev9", "Legacy", 9_000)], 100)
            .unwrap();
        // The resource changes: ev1 retitled, ev2 gone, ev4 new.
        let counts = db
            .reconcile_caldav_resource(
                &src.id,
                "https://cal.test/u/a.ics",
                &[
                    item("ev1", "Standup moved", 1_000),
                    item("ev4", "New", 4_000),
                ],
                200,
            )
            .unwrap();
        assert_eq!(
            counts,
            ReconcileCounts {
                inserted: 1,
                updated: 1,
                withdrawn: 1,
            }
        );
        let titles: Vec<(String, String)> = db
            .suggestions(true)
            .unwrap()
            .into_iter()
            .map(|s| (s.external_uid, s.title))
            .collect();
        assert!(titles.contains(&("ev1".into(), "Standup moved".into())));
        assert!(titles.contains(&("ev4".into(), "New".into())));
        assert!(
            titles.contains(&("ev3".into(), "Other".into())),
            "another resource's pending suggestion is untouched: {titles:?}"
        );
        assert!(
            titles.contains(&("ev9".into(), "Legacy".into())),
            "an unattributed row is never withdrawn: {titles:?}"
        );
        assert!(
            !titles.iter().any(|(uid, _)| uid == "ev2"),
            "the dropped occurrence is withdrawn: {titles:?}"
        );
        // A re-fetch that matches pending rows changes nothing.
        let counts = db
            .reconcile_caldav_resource(
                &src.id,
                "https://cal.test/u/a.ics",
                &[
                    item("ev1", "Standup moved", 1_000),
                    item("ev4", "New", 4_000),
                ],
                300,
            )
            .unwrap();
        assert_eq!(counts, ReconcileCounts::default());
    }

    #[test]
    fn reconcile_never_revives_a_decision() {
        use timetrack_core::{ExternalItem, ExternalKind};
        let db = SqliteStore::open_file(&dir("reconcile-decided").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        let item = ExternalItem {
            uid: "ev1".into(),
            kind: ExternalKind::Event,
            title: "Review".into(),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        db.reconcile_caldav_resource(
            &src.id,
            "https://cal.test/u/a.ics",
            std::slice::from_ref(&item),
            100,
        )
        .unwrap();
        let pending = db.suggestions(true).unwrap();
        assert_eq!(pending.len(), 1);
        db.decide_suggestion(&pending[0].id, "dismissed", None, 200)
            .unwrap();
        // The event reappears remotely (or the token died and a full
        // resync replays it): the dismissal stands, nothing new.
        let counts = db
            .reconcile_caldav_resource(
                &src.id,
                "https://cal.test/u/a.ics",
                std::slice::from_ref(&item),
                300,
            )
            .unwrap();
        assert_eq!(counts, ReconcileCounts::default());
        assert!(db.suggestions(true).unwrap().is_empty());
        assert_eq!(db.suggestions(false).unwrap().len(), 1);
        // Withdrawing the resource keeps the dismissal too.
        assert_eq!(
            db.withdraw_caldav_resource(&src.id, "https://cal.test/u/a.ics")
                .unwrap(),
            0
        );
        let all = db.suggestions(false).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "dismissed");
    }

    #[test]
    fn withdraw_keeps_confirmed_entries_and_drops_validators() {
        use timetrack_core::{ExternalItem, ExternalKind};
        let db = SqliteStore::open_file(&dir("withdraw").join("store.db")).unwrap();
        let src = db
            .add_source("caldav", "https://cal.test/u/", None, "Cal", 100)
            .unwrap();
        let item = |uid: &str| ExternalItem {
            uid: uid.into(),
            kind: ExternalKind::Event,
            title: uid.into(),
            started_at: 1_000,
            ended_at: 61_000,
            recurring: false,
        };
        db.reconcile_caldav_resource(
            &src.id,
            "https://cal.test/u/a.ics",
            &[item("ev1"), item("ev2")],
            100,
        )
        .unwrap();
        db.upsert_caldav_resource(
            &src.id,
            "https://cal.test/u/a.ics",
            Some("\"1\""),
            Some("h"),
            100,
        )
        .unwrap();
        let pending = db.suggestions(true).unwrap();
        db.decide_suggestion(&pending[0].id, "confirmed", Some("e1"), 200)
            .unwrap();
        assert_eq!(
            db.withdraw_caldav_resource(&src.id, "https://cal.test/u/a.ics")
                .unwrap(),
            1,
            "only the still-pending suggestion withdraws"
        );
        let all = db.suggestions(false).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "confirmed");
        assert_eq!(all[0].entry_id.as_deref(), Some("e1"));
        assert!(
            db.caldav_resources(&src.id).unwrap().is_empty(),
            "the validator row goes with the resource"
        );
    }

    #[test]
    fn a_failed_remint_leaves_the_imported_store_untouched() {
        let mut store = sample();
        let before = store.clone();
        let mut calls = 0;
        let err = remint_store_ids(&mut store, &mut || -> StoreResult<String> {
            calls += 1;
            if calls > 1 {
                return Err(StoreError::Invalid {
                    path: "test".into(),
                    reason: "boom".into(),
                });
            }
            Ok("00000000-0000-4000-8000-000000000001".into())
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(store, before);
    }

    fn v5_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        conn.execute_batch(SCHEMA_V5).unwrap();
        let proj = "11111111-1111-4111-8111-111111111111";
        let ent = "22222222-2222-4222-8222-222222222222";
        let already_uuid_src = "33333333-3333-4333-8333-333333333333";
        conn.execute(
            "INSERT INTO projects(id, name, colour, archived, created_at, updated_at, device_id)
             VALUES (?1, 'Work', 255, 0, 1, 2, 'dev')",
            params![proj],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries(id, project_id, description, started_at, ended_at, source, note,
                 created_at, updated_at, device_id)
             VALUES (?1, ?2, 'review', 1000, 61000, 2, NULL, 1, 2, 'dev')",
            params![ent, proj],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_sources(id, kind, locator, project_id, label,
                 cred_ref, cred_username, created_at, updated_at, device_id)
             VALUES ('src-legacy-1', 'caldav', 'https://cal.test/1/', ?1, 'One',
                 'caldav/src-legacy-1', 'alice', 1, 2, 'dev'),
                    (?2, 'ics_file', '/tmp/a.ics', ?1, 'Two',
                 NULL, NULL, 1, 2, 'dev')",
            params![proj, already_uuid_src],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO suggestions(id, source_id, external_uid, kind, title, started_at,
                 ended_at, status, entry_id, fetched_at, decided_at, resource_href)
             VALUES ('sg-1', 'src-legacy-1', 'ev1', 'event', 'Review', 1000, 61000,
                 'confirmed', ?1, 1000, 2000, '/1/a.ics'),
                    ('sg-2', 'src-deleted-old', 'ev2', 'event', 'Orphan', 2000, 62000,
                 'dismissed', NULL, 1000, 2000, NULL),
                    ('sg-3', ?2, 'ev3', 'event', 'Kept', 3000, 63000,
                 'pending', NULL, 1000, NULL, NULL)",
            params![ent, already_uuid_src],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO caldav_sync(source_id, sync_token, ctag, updated_at)
             VALUES ('src-legacy-1', 'tok-1', 'ctag-1', 10)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO caldav_resources(source_id, href, etag, content_hash, seen_at)
             VALUES ('src-legacy-1', '/1/a.ics', '\"v1\"', 'h1', 10)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migration_v5_to_v6_remints_source_ids_and_keeps_cred_ref() {
        let path = dir("v5-v6").join("store.db");
        v5_database(&path);
        let db = SqliteStore::open_file(&path).unwrap();

        let store = db.load().unwrap();
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.total_ms(), 60_000);

        let sources = db.sources().unwrap();
        assert_eq!(sources.len(), 2);
        for s in &sources {
            assert!(is_uuid(&s.id), "source id must be UUID: {}", s.id);
            assert!(
                !s.id.starts_with("src-"),
                "no src-* id may remain: {}",
                s.id
            );
        }
        let one = sources.iter().find(|s| s.label == "One").unwrap();
        let two = sources.iter().find(|s| s.label == "Two").unwrap();
        assert_eq!(
            two.id, "33333333-3333-4333-8333-333333333333",
            "already-UUID source id is untouched"
        );
        // Per-device OS keyring pointer must stay intact across remint.
        assert_eq!(one.cred_ref.as_deref(), Some("caldav/src-legacy-1"));
        assert_eq!(one.cred_username.as_deref(), Some("alice"));

        let suggestions = db.suggestions(false).unwrap();
        for sg in &suggestions {
            assert!(
                is_uuid(&sg.source_id),
                "suggestion {} source_id must be UUID: {}",
                sg.id,
                sg.source_id
            );
            assert!(!sg.source_id.starts_with("src-"));
        }
        let sg1 = suggestions.iter().find(|s| s.id == "sg-1").unwrap();
        let sg2 = suggestions.iter().find(|s| s.id == "sg-2").unwrap();
        let sg3 = suggestions.iter().find(|s| s.id == "sg-3").unwrap();
        assert_eq!(sg1.source_id, one.id);
        assert_ne!(sg2.source_id, one.id);
        assert_eq!(sg3.source_id, two.id);

        assert_eq!(
            db.caldav_sync_state(&one.id)
                .unwrap()
                .unwrap()
                .sync_token
                .as_deref(),
            Some("tok-1")
        );
        assert_eq!(db.caldav_resources(&one.id).unwrap().len(), 1);
        assert_eq!(db.caldav_sync_state("src-legacy-1").unwrap(), None);
        assert!(db.caldav_resources("src-legacy-1").unwrap().is_empty());
    }

    #[test]
    fn a_failed_v5_to_v6_source_remint_rolls_back_to_an_untouched_v5() {
        let path = dir("v5-v6-rollback").join("store.db");
        v5_database(&path);
        let before = dump_all(&path);

        // Fail on the 2nd mint (after the 1st source id was already mapped,
        // before/during orphan suggestion remint): transaction must roll back.
        let mut calls = 0;
        let mut failing = || -> StoreResult<String> {
            calls += 1;
            if calls > 1 {
                return Err(StoreError::Invalid {
                    path: "test".into(),
                    reason: "boom".into(),
                });
            }
            Ok(format!("00000000-0000-4000-8000-{calls:012}"))
        };
        let mut conn = Connection::open(&path).unwrap();
        let err = migrate_with(&mut conn, &path, &mut failing).unwrap_err();
        assert!(err.to_string().contains("boom"));
        drop(conn);

        let v: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 5, "a failed migration keeps version 5");
        assert_eq!(
            dump_all(&path),
            before,
            "rollback leaves every row identical"
        );

        // Subsequent open recovers cleanly.
        let db = SqliteStore::open_file(&path).unwrap();
        assert!(db.sources().unwrap().iter().all(|s| is_uuid(&s.id)));
    }

    #[test]
    fn a_failed_legacy_json_remint_during_open_at_leaves_no_db_and_touches_no_json() {
        let d = dir("legacy-rollback");
        let json_path = d.join("store.json");
        let json = JsonStore::new(json_path.clone());
        json.save(&sample()).unwrap();
        let before = std::fs::read(&json_path).unwrap();

        let mut calls = 0;
        let err = SqliteStore::open_at_with(&json_path, &mut || {
            calls += 1;
            if calls > 1 {
                return Err(StoreError::Invalid {
                    path: "test".into(),
                    reason: "boom".into(),
                });
            }
            Ok("00000000-0000-4000-8000-000000000001".into())
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert!(
            !SqliteStore::db_path_for(&json_path).exists(),
            "failed import creates no database file"
        );
        assert_eq!(
            std::fs::read(&json_path).unwrap(),
            before,
            "legacy JSON untouched"
        );
    }

    #[test]
    fn remove_source_writes_source_tombstone_and_readd_mints_fresh_uuid() {
        let db = SqliteStore::open_file(&dir("src-tombstone").join("store.db")).unwrap();
        let src = db
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal", 100)
            .unwrap();
        assert!(
            is_uuid(&src.id),
            "newly added source id is UUIDv4: {}",
            src.id
        );
        assert!(db.remove_source(&src.id, 5_000).unwrap());

        let stones = db.tombstones().unwrap();
        assert_eq!(stones.len(), 1);
        assert_eq!(stones[0].id, src.id);
        assert_eq!(stones[0].kind, "source");
        assert_eq!(stones[0].deleted_at, 5_000);
        assert_eq!(stones[0].device_id, db.device_id());

        // Re-adding the same locator creates a distinct fresh UUID, never
        // colliding with the removed source's tombstone.
        let readded = db
            .add_source("ics_file", "/tmp/cal.ics", None, "Cal", 6_000)
            .unwrap();
        assert!(is_uuid(&readded.id));
        assert_ne!(readded.id, src.id);
    }

    #[test]
    fn migration_v6_to_v7_and_sync_merge_preserve_local_cred_and_conflicts() {
        let path = dir("v6-v7").join("store.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V4).unwrap();
            conn.execute_batch(SCHEMA_V5).unwrap();
            conn.execute(
                "INSERT INTO external_sources(id, kind, locator, project_id, label,
                     cred_ref, cred_username, created_at, updated_at, device_id)
                 VALUES ('11111111-1111-4111-8111-111111111111', 'caldav', 'https://cal.test/1/', NULL, 'Cal',
                     'caldav/11111111-1111-4111-8111-111111111111', 'alice', 10, 10, 'dev-a'),
                        ('22222222-2222-4222-8222-222222222222', 'caldav', 'https://cal.test/2/', NULL, 'Del',
                     'caldav/22222222-2222-4222-8222-222222222222', 'bob', 10, 10, 'dev-a')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 6).unwrap();
        }
        let db = SqliteStore::open_file(&path).unwrap();
        let v: i64 = db
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        db.set_sync_folder("/tmp/sync").unwrap();
        assert_eq!(
            db.sync_config().unwrap().folder.as_deref(),
            Some("/tmp/sync")
        );

        let write = SyncMergeWrite {
            upsert_sources: vec![ExternalSourceRow {
                source: ExternalSource {
                    id: "11111111-1111-4111-8111-111111111111".into(),
                    kind: "caldav".into(),
                    locator: "https://cal.test/1/".into(),
                    project_id: None,
                    label: "Renamed remotely".into(),
                    cred_ref: None,
                    cred_username: None,
                },
                created_at: 10,
                updated_at: 200,
                device_id: "dev-b".into(),
            }],
            delete_sources: vec!["22222222-2222-4222-8222-222222222222".into()],
            new_conflicts: vec![SyncConflictRow {
                id: "cccccccc-0000-4000-8000-000000000001".into(),
                kind: "project".into(),
                record_id: "pppppppp-0000-4000-8000-000000000001".into(),
                recorded_at: 300,
                loser_updated_at: 150,
                loser_device_id: db.device_id().into(),
                winner_updated_at: 250,
                winner_device_id: "dev-b".into(),
                loser_payload: r#"{"kind":"project","id":"pppppppp-0000-4000-8000-000000000001","name":"Local","colour":null,"archived":false,"created_at":100,"updated_at":150,"device_id":"d"}"#.into(),
                restored_at: None,
            }],
            upsert_peers: vec![SyncPeerRow {
                device_id: "dev-b".into(),
                last_published_at: 250,
                last_merged_through: 50,
                last_seen_at: 300,
                quarantined: false,
                last_warning: None,
            }],
            merged_through: Some(250),
            last_sync_at: Some(300),
            last_error: Some(None),
            ..Default::default()
        };

        let purged = db.apply_sync_merge(&write).unwrap();
        assert_eq!(
            purged,
            vec!["caldav/22222222-2222-4222-8222-222222222222".to_string()]
        );

        let kept = db
            .source("11111111-1111-4111-8111-111111111111")
            .unwrap()
            .unwrap();
        assert_eq!(kept.label, "Renamed remotely");
        assert_eq!(
            kept.cred_ref.as_deref(),
            Some("caldav/11111111-1111-4111-8111-111111111111"),
            "local OS credential pointer must survive remote source update"
        );
        assert_eq!(kept.cred_username.as_deref(), Some("alice"));

        assert_eq!(db.sync_conflicts(false).unwrap().len(), 1);
        assert_eq!(db.sync_peers().unwrap().len(), 1);

        // Restoring the conflict re-saves the project and marks the journal row restored.
        db.restore_sync_conflict(
            "cccccccc-0000-4000-8000-000000000001",
            400,
            &RestoredRecord::Project(Project {
                id: "pppppppp-0000-4000-8000-000000000001".into(),
                name: "Local".into(),
                colour: None,
                archived: false,
            }),
        )
        .unwrap();
        assert!(db.sync_conflicts(false).unwrap().is_empty());
        assert_eq!(db.sync_conflicts(true).unwrap()[0].restored_at, Some(400));
        assert_eq!(db.load().unwrap().projects[0].name, "Local");

        // Unpairing clears sync config & peers but keeps the conflict journal and data.
        db.clear_sync_config().unwrap();
        assert_eq!(db.sync_config().unwrap().folder, None);
        assert!(db.sync_peers().unwrap().is_empty());
        assert_eq!(db.sync_conflicts(true).unwrap().len(), 1);
        assert_eq!(db.load().unwrap().projects.len(), 1);
    }
}
