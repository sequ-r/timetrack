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
pub const SCHEMA_VERSION: i64 = 2;

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
    /// `"entry"` or `"project"`.
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
        let db_path = Self::db_path_for(json_path);
        let existed = db_path.exists();
        let store = Self::open_file(&db_path)?;
        if !existed && json_path.exists() {
            let legacy = JsonStore::new(json_path.to_path_buf()).load()?;
            store.save_all(&legacy, system_now_ms())?;
            store.set_meta("migrated_from", "store.json")?;
        }
        Ok(store)
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
        let id = mint_id(&conn, "src-").map_err(|e| sqlite(&self.path, e))?;
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
                "SELECT id, kind, locator, project_id, label FROM external_sources ORDER BY seq",
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

/// Numbered schema migrations, applied in order on open. Each step runs
/// in its own transaction with the `user_version` bump inside it, so a
/// crash mid-migration leaves the previous version whole.
fn migrate(conn: &mut Connection, path: &Path) -> StoreResult<()> {
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
        tx.execute_batch(
            "CREATE TABLE meta (
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
             PRAGMA user_version = 1;",
        )
        .map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
        version = 1;
    }
    if version < 2 {
        // §19: external sources and their suggestions. Pure additions —
        // no existing row moves, so a v1 database gains two empty tables.
        let tx = conn.transaction().map_err(|e| sqlite(path, e))?;
        tx.execute_batch(
            "CREATE TABLE external_sources (
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
             PRAGMA user_version = 2;",
        )
        .map_err(|e| sqlite(path, e))?;
        tx.commit().map_err(|e| sqlite(path, e))?;
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
        assert_eq!(db.load().unwrap(), after, "checkpoint + journal imported");
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
        let mut newer = after.clone();
        newer.entries[0].description = "in db".into();
        db.apply(&Journal::between(&after, &newer), 6_000).unwrap();
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
}
