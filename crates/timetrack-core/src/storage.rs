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
//! Saves are atomic: the document is written to a temporary file in the same
//! directory and renamed over the target. A rename within a directory is
//! atomic, so a crash mid-write leaves the previous good file intact rather
//! than a truncated one. The service persists *before* acknowledging a
//! mutation, so an acknowledged entry is on disk.

use crate::model::{STORE_VERSION, Store};
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
                // A zero-length file is a crash artifact, not real data.
                if text.trim().is_empty() {
                    return Ok(Store::default());
                }
                let store: Store =
                    serde_json::from_str(&text).map_err(|e| StorageError::Invalid {
                        path: shown.clone(),
                        reason: e.to_string(),
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
        let mut doc = store.clone();
        doc.version = STORE_VERSION;
        let text = serde_json::to_string_pretty(&doc).map_err(|e| StorageError::Invalid {
            path: shown.clone(),
            reason: e.to_string(),
        })?;

        // A sibling temp file, so the rename stays within one filesystem and
        // is therefore atomic.
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(write)?;
            f.write_all(text.as_bytes()).map_err(write)?;
            // Durability before the rename: without the sync, a crash can
            // leave the renamed file pointing at unwritten blocks.
            f.sync_all().map_err(write)?;
        }
        std::fs::rename(&tmp, &self.path).map_err(write)?;
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
    fn an_empty_file_loads_as_empty() {
        // A zero-length file is what a crash mid-write would leave behind if
        // the rename were not atomic; treat it as "nothing tracked yet".
        let dir = tmpdir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(&path, "").unwrap();
        let store = JsonStore::new(path);
        assert!(store.load().unwrap().entries.is_empty());
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
