/* core/storage.rs
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

//! JSON-file persistence for [`Store`].

use crate::model::Store;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed store at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl StorageError {
    fn io(path: &Path, source: io::Error) -> Self {
        StorageError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Reads and writes the store as JSON.
///
/// Writes go to a sibling temp file and are then renamed, so an interrupted
/// write cannot leave a truncated store behind.
#[derive(Debug, Clone)]
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        JsonStore { path: path.into() }
    }

    /// The default location, honouring `XDG_DATA_HOME`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
                home.join(".local").join("share")
            });
        base.join("timetrack").join("store.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the store. A missing file is an empty store, not an error.
    pub fn load(&self) -> Result<Store, StorageError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| StorageError::Parse {
                path: self.path.clone(),
                source,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Store::default()),
            Err(source) => Err(StorageError::io(&self.path, source)),
        }
    }

    /// Persist the store atomically.
    pub fn save(&self, store: &Store) -> Result<(), StorageError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| StorageError::io(parent, e))?;
        }
        let text = serde_json::to_string_pretty(store).expect("Store is always serializable");
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|e| StorageError::io(&tmp, e))?;
        fs::rename(&tmp, &self.path).map_err(|e| StorageError::io(&self.path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Entry;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tt-core-test-{name}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn missing_file_loads_as_empty_store() {
        let dir = tmpdir("missing");
        let store = JsonStore::new(dir.join("store.json")).load().unwrap();
        assert!(store.entries.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("nested").join("store.json");
        let store = JsonStore::new(&path);

        let mut data = Store::default();
        data.entries.push(Entry {
            id: "e1".into(),
            description: "test".into(),
            started_at: 10,
            ended_at: Some(20),
        });
        store.save(&data).unwrap();
        assert!(path.exists(), "save must create parent directories");

        let back = store.load().unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tmpdir("notemp");
        let path = dir.join("store.json");
        let store = JsonStore::new(&path);
        store.save(&Store::default()).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_silent_reset() {
        // Losing a user's tracked time because of a parse hiccup would be
        // far worse than reporting the failure.
        let dir = tmpdir("malformed");
        let path = dir.join("store.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            JsonStore::new(&path).load().unwrap_err(),
            StorageError::Parse { .. }
        ));
    }

    #[test]
    fn default_path_is_under_xdg_data_home() {
        // SAFETY: single-threaded test that restores the previous value.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-test") };
        let path = JsonStore::default_path();
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        assert_eq!(path, PathBuf::from("/tmp/xdg-test/timetrack/store.json"));
    }
}
