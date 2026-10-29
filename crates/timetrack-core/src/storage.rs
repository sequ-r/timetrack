/* core/storage.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Loading and saving the store.
//!
//! Storage lives in the service, not in the rules: the core stays pure so it
//! can be tested without a filesystem, and so the CLI never links it.
//!
//! # Durability
//!
//! Saves are atomic: the document is written to a uniquely named temporary
//! file in the same directory, synced, renamed over the target, and the
//! parent directory is synced so the rename itself survives a crash. A
//! rename within a directory is atomic, so a crash mid-write leaves the
//! previous good file intact rather than a truncated one. The service
//! persists *before* acknowledging a mutation, so an acknowledged entry is
//! on disk.

use crate::model::{Entry, Project, STORE_VERSION, Store};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Why a load or save failed.
///
/// A local type rather than `anyhow`: the core has no opinion about error
/// handling frameworks, and a `thiserror` enum is enough for the three
/// failures that can actually happen here.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The file exists but is not a store this build can read.
    ///
    /// Kept distinct from `Read` on purpose: a corrupt or too-new file means
    /// the user's data is still on disk and must not be overwritten, so the
    /// service has to refuse loudly rather than start empty.
    #[error("{path} could not be read as a TimeTrack store: {reason}")]
    Invalid { path: String, reason: String },
}

/// Explain an unreadable store, naming the remedy when the shape is
/// recognisable.
///
/// A stopwatch-era file (entries without `project_id`) predates project
/// attribution, so there is nothing to migrate it onto; the way forward is
/// moving it aside and starting fresh. Anything else is reported as-is: a
/// corrupt or newer file may still hold data worth keeping, where "start
/// fresh" would be the wrong advice.
fn invalid_reason(text: &str, error: &serde_json::Error) -> String {
    if looks_like_legacy_store(text) {
        format!(
            "{error}; this is the pre-project (stopwatch-era) format, which cannot be migrated -- move the file aside to start fresh"
        )
    } else {
        error.to_string()
    }
}

/// Whether `text` parses as JSON shaped like the pre-project store: a
/// non-empty entry list with no `project_id` anywhere in it.
///
/// The legacy writer never emitted `project_id`, so `all` (not `any`) is the
/// test: a mix of shaped and shapeless entries is hand-editing or
/// corruption, not the legacy format, and must keep the generic message.
fn looks_like_legacy_store(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(entries) = value.get("entries").and_then(|v| v.as_array()) else {
        return false;
    };
    !entries.is_empty() && entries.iter().all(|e| e.get("project_id").is_none())
}

/// Result alias for this module, named to avoid shadowing the prelude's
/// two-parameter `Result`.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

/// A store backed by one JSON file.
#[derive(Debug, Clone)]
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    /// Store at an explicit path.
    pub fn new(path: PathBuf) -> Self {
        JsonStore { path }
    }

    /// The conventional location: `$XDG_DATA_HOME/timetrack/store.json`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                home.join(".local").join("share")
            });
        base.join("timetrack").join("store.json")
    }

    /// Where this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the store, treating a missing file as a fresh one.
    ///
    /// A malformed file is an error rather than a silent reset: quietly
    /// discarding a user's tracked time would be the worst possible failure.
    pub fn load(&self) -> StorageResult<Store> {
        let shown = self.path.display().to_string();
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                // A zero-length file is a crash artifact, not "nothing
                // tracked yet" -- that state is a *missing* file. Treating
                // it as fresh would let the next save silently replace
                // whatever it was meant to hold, so refuse like any other
                // corrupt file (#100).
                if text.trim().is_empty() {
                    return Err(StorageError::Invalid {
                        path: shown,
                        reason: "file is empty".to_string(),
                    });
                }
                let store: Store =
                    serde_json::from_str(&text).map_err(|e| StorageError::Invalid {
                        path: shown.clone(),
                        reason: invalid_reason(&text, &e),
                    })?;
                if !store.is_readable() {
                    return Err(StorageError::Invalid {
                        path: shown,
                        reason: format!(
                            "written by a newer TimeTrack (format v{}, this build reads v{})",
                            store.version, STORE_VERSION
                        ),
                    });
                }
                Ok(store)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(source) => Err(StorageError::Read {
                path: shown,
                source,
            }),
        }
    }

    /// Write the store atomically, creating parent directories as needed.
    pub fn save(&self, store: &Store) -> StorageResult<()> {
        let shown = self.path.display().to_string();
        let write = |source: std::io::Error| StorageError::Write {
            path: shown.clone(),
            source,
        };

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(write)?;
        }
        // The document as written: the store's own lists under the current
        // format version. Borrowing them into this view stamps the version
        // without cloning the whole store first -- a save happens on every
        // acknowledged mutation, so a deep copy per save made each tap
        // cost a full duplicate of the history. Field order matches
        // `Store`'s, so the bytes on disk are unchanged.
        #[derive(serde::Serialize)]
        struct OnDisk<'a> {
            version: u32,
            projects: &'a [Project],
            entries: &'a [Entry],
        }
        let doc = OnDisk {
            version: STORE_VERSION,
            projects: &store.projects,
            entries: &store.entries,
        };

        // A sibling temp file, so the rename stays within one filesystem and
        // is therefore atomic. Unique per save (#100): a fixed name lets
        // two racing writers truncate or rename each other's document.
        static SAVE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SAVE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.tmp-{}-{n}", std::process::id()));
        let written = (|| -> StorageResult<()> {
            let f = std::fs::File::create(&tmp).map_err(write)?;
            // Serialise straight into the file: building the whole
            // document as one string first held a second full-size copy
            // of the history next to the store itself. The buffer is
            // wide because the pretty writer emits many small pieces:
            // at the default 8 KiB a multi-megabyte store costs hundreds
            // of write calls where a handful do.
            let mut w = std::io::BufWriter::with_capacity(128 * 1024, f);
            serde_json::to_writer_pretty(&mut w, &doc).map_err(|e| {
                if e.is_io() {
                    write(e.into())
                } else {
                    StorageError::Invalid {
                        path: shown.clone(),
                        reason: e.to_string(),
                    }
                }
            })?;
            w.flush().map_err(write)?;
            // Durability before the rename: without the sync, a crash can
            // leave the renamed file pointing at unwritten blocks.
            w.get_ref().sync_all().map_err(write)?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(write(e));
        }
        // The rename itself is only durable once the directory entry is
        // synced; without this a crash can still lose the new name (#100).
        #[cfg(unix)]
        {
            let parent = match self.path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            let dir = std::fs::File::open(parent).map_err(write)?;
            dir.sync_all().map_err(write)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entry, EntrySource, Project};

    fn tmpdir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("timetrack-store-test-{tag}-{}", std::process::id()));
        d
    }

    fn sample() -> Store {
        let mut s = Store::default();
        s.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: Some(0x2ea043),
            archived: false,
        });
        s.entries.push(Entry {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "work".into(),
            started_at: 1_000,
            ended_at: 4_500,
            source: EntrySource::Manual,
            note: None,
        });
        s
    }

    #[test]
    fn a_missing_file_loads_as_a_fresh_store() {
        let dir = tmpdir("missing");
        let store = JsonStore::new(dir.join("nope.json"));
        let loaded = store.load().unwrap();
        assert!(loaded.entries.is_empty());
        assert!(loaded.projects.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("store.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(path);
        let mut expected = sample();
        // `save` stamps the current format version, so the round trip is
        // expected to come back stamped -- that is the point of stamping.
        expected.version = STORE_VERSION;
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_stamps_the_current_version() {
        let dir = tmpdir("version");
        let path = dir.join("store.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(path.clone());
        let mut s = sample();
        s.version = 0;
        store.save(&s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("\"version\": {STORE_VERSION}")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_file_is_refused_like_other_corruption() {
        // A zero-length file is a crash artifact, not "nothing tracked
        // yet": loading it as fresh would let the next save silently
        // replace whatever the file was meant to hold (#100).
        let dir = tmpdir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(&path, "").unwrap();
        let store = JsonStore::new(path);
        let err = store.load().unwrap_err();
        assert!(
            matches!(err, StorageError::Invalid { .. }),
            "empty file must be Invalid, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_do_not_clobber_each_others_temp_file() {
        // Unique temp names (#100): two writers racing the same store must
        // not share one temp file, or one can rename/truncate the other's
        // half-written document.
        let dir = tmpdir("concurrent");
        let path = dir.join("store.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(path);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let data = sample();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        store.save(&data).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut expected = sample();
        expected.version = STORE_VERSION;
        assert_eq!(store.load().unwrap(), expected);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_silent_reset() {
        let dir = tmpdir("bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let store = JsonStore::new(path);
        assert!(
            store.load().is_err(),
            "must not discard tracked time quietly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_format_version_is_refused() {
        let dir = tmpdir("newer");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        let text = format!("{{\"version\":{}}}", STORE_VERSION + 1);
        std::fs::write(&path, text).unwrap();
        assert!(JsonStore::new(path).load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_store_names_the_remedy() {
        // The stopwatch-era shape: entries with no `project_id` and no
        // project table. The error must say what to do, not just what is
        // wrong, because the service will not start until the file moves.
        let dir = tmpdir("legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(
            &path,
            r#"{"entries": [{"id": "e1", "description": "old", "started_at": 1000, "ended_at": 2000}]}"#,
        )
        .unwrap();
        let err = format!("{}", JsonStore::new(path).load().unwrap_err());
        assert!(
            err.contains("cannot be migrated"),
            "must name the dead end: {err}"
        );
        assert!(
            err.contains("move the file aside"),
            "must name the remedy: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbled_store_gets_no_migration_advice() {
        // "Start fresh" would discard real data here, so the legacy hint
        // must not fire: unparseable JSON and half-shaped entries keep the
        // generic message.
        let dir = tmpdir("garbled");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let not_json = dir.join("store.json");
        std::fs::write(&not_json, "{ this is not json").unwrap();
        let err = format!("{}", JsonStore::new(not_json).load().unwrap_err());
        assert!(
            !err.contains("move the file aside"),
            "must not advise discarding possibly real data: {err}"
        );
        let mixed = dir.join("mixed.json");
        std::fs::write(
            &mixed,
            r#"{"entries": [
                {"id": "e1", "project_id": "p1", "description": "x", "started_at": 1, "ended_at": 2, "source": "Manual"},
                {"id": "e2", "description": "hand-edited", "started_at": 3, "ended_at": 4}
            ]}"#,
        )
        .unwrap();
        let err = format!("{}", JsonStore::new(mixed).load().unwrap_err());
        assert!(
            !err.contains("move the file aside"),
            "a mixed file is corruption, not the legacy format: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tmpdir("nested");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(dir.join("a").join("b").join("store.json"));
        store.save(&sample()).unwrap();
        assert!(store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tmpdir("tmp");
        let path = dir.join("store.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(path);
        store.save(&sample()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
