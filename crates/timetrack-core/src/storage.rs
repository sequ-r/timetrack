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
//!
//! # Journal and checkpoint (#103, ADR 0010)
//!
//! The pretty-JSON document above is now the *checkpoint*. A mutation no
//! longer rewrites it: the service appends one [`Journal`] line — the diff
//! of that one mutation — to a sibling journal file and syncs it before
//! acknowledging, so a tap costs bytes proportional to the change, not to
//! the whole history. Loading reads the checkpoint and replays the journal
//! over it; once the journal grows past [`JOURNAL_CHECKPOINT_BYTES`] the
//! checkpoint is rewritten (the atomic save) and the journal truncated.
//! Replay is idempotent, so a crash between the checkpoint rename and the
//! truncation replays to the same store, and a torn final line is by
//! construction a mutation that was never acknowledged.

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

/// Journal size past which a mutation also rewrites the checkpoint.
///
/// Big enough that ordinary days (a few hundred bytes of journal each)
/// never trigger a whole-store rewrite, small enough that startup replay
/// stays a few hundred records beside the checkpoint read.
pub const JOURNAL_CHECKPOINT_BYTES: u64 = 256 * 1024;

/// One mutation's diff: the entries and projects it created or changed,
/// and the ids it removed (#103).
///
/// The journal stores diffs, not operations, so it needs no encoding per
/// rule: anything the rules can do to a [`Store`] is expressible as
/// upserts plus removals. Applying a journal is idempotent — upserting
/// the same values and removing already-removed ids changes nothing —
/// which is what makes checkpoint-then-truncate crash-safe without a
/// two-phase protocol.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Journal {
    /// Entries created or changed, in store order.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Entry ids removed.
    #[serde(default)]
    pub removed_entries: Vec<String>,
    /// Projects created or changed, in store order.
    #[serde(default)]
    pub projects: Vec<Project>,
    /// Project ids removed.
    #[serde(default)]
    pub removed_projects: Vec<String>,
}

impl Journal {
    /// The diff that turns `before` into `after`.
    ///
    /// Indexed by id rather than scanned pairwise: this runs on every
    /// mutation over the whole history, and a quadratic diff would hand
    /// back the linear-in-history cost the journal exists to remove.
    pub fn between(before: &Store, after: &Store) -> Journal {
        use std::collections::HashMap;
        let before_entries: HashMap<&str, &Entry> =
            before.entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let after_entries: HashMap<&str, &Entry> =
            after.entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let before_projects: HashMap<&str, &Project> =
            before.projects.iter().map(|p| (p.id.as_str(), p)).collect();
        let after_projects: HashMap<&str, &Project> =
            after.projects.iter().map(|p| (p.id.as_str(), p)).collect();
        Journal {
            entries: after
                .entries
                .iter()
                .filter(|e| before_entries.get(e.id.as_str()) != Some(e))
                .cloned()
                .collect(),
            removed_entries: before
                .entries
                .iter()
                .filter(|e| !after_entries.contains_key(e.id.as_str()))
                .map(|e| e.id.clone())
                .collect(),
            projects: after
                .projects
                .iter()
                .filter(|p| before_projects.get(p.id.as_str()) != Some(p))
                .cloned()
                .collect(),
            removed_projects: before
                .projects
                .iter()
                .filter(|p| !after_projects.contains_key(p.id.as_str()))
                .map(|p| p.id.clone())
                .collect(),
        }
    }

    /// Whether this diff changes nothing (a mutation the rules allowed
    /// as a no-op): appending it would only grow the journal.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.removed_entries.is_empty()
            && self.projects.is_empty()
            && self.removed_projects.is_empty()
    }

    /// Apply this diff to `store`: removals first, then upserts replace
    /// by id in place or append, preserving the store's order.
    pub fn apply_to(&self, store: &mut Store) {
        store
            .entries
            .retain(|e| !self.removed_entries.contains(&e.id));
        for entry in &self.entries {
            match store.get_mut(&entry.id) {
                Some(existing) => *existing = entry.clone(),
                None => store.entries.push(entry.clone()),
            }
        }
        store
            .projects
            .retain(|p| !self.removed_projects.contains(&p.id));
        for project in &self.projects {
            match store.projects.iter_mut().find(|p| p.id == project.id) {
                Some(existing) => *existing = project.clone(),
                None => store.projects.push(project.clone()),
            }
        }
    }
}

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

    /// The journal sibling: `store.json` keeps its checkpoint at
    /// `store.json` and its journal at `store.json.journal` (#103), so
    /// copying the checkpoint alone still copies a readable (if older)
    /// store, and the pair is obvious in a directory listing.
    pub fn journal_path(&self) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(".journal");
        PathBuf::from(name)
    }

    /// Read the store: the checkpoint with the journal replayed over it,
    /// treating a missing checkpoint as a fresh store (#103).
    ///
    /// A malformed file is an error rather than a silent reset: quietly
    /// discarding a user's tracked time would be the worst possible failure.
    /// The one exception is a torn final journal line — a mutation that
    /// crashed before its line (or its newline) landed was never
    /// acknowledged, so dropping it loses nothing anyone was promised.
    pub fn load(&self) -> StorageResult<Store> {
        let mut store = self.load_checkpoint()?;
        let journal = self.journal_path();
        let shown = journal.display().to_string();
        let text = match std::fs::read_to_string(&journal) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(store),
            Err(source) => {
                return Err(StorageError::Read {
                    path: shown,
                    source,
                });
            }
        };
        let terminated = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Journal>(line) {
                Ok(record) => record.apply_to(&mut store),
                Err(e) => {
                    if i + 1 == lines.len() && !terminated {
                        break;
                    }
                    return Err(StorageError::Invalid {
                        path: shown,
                        reason: format!("journal line {}: {e}", i + 1),
                    });
                }
            }
        }
        Ok(store)
    }

    /// Read only the checkpoint document.
    fn load_checkpoint(&self) -> StorageResult<Store> {
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

    /// Append one mutation's journal record and sync it (#103).
    ///
    /// This is the persist-before-acknowledge write on the mutation path:
    /// one JSON line, so the cost tracks the change, not the history. An
    /// empty diff writes nothing. The directory is synced when the file
    /// is first created so the new name itself survives a crash.
    pub fn append(&self, record: &Journal) -> StorageResult<()> {
        if record.is_empty() {
            return Ok(());
        }
        let journal = self.journal_path();
        let shown = journal.display().to_string();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::Write {
                path: shown.clone(),
                source,
            })?;
        }
        let existed = journal.exists();
        let mut line = serde_json::to_string(record).map_err(|e| StorageError::Invalid {
            path: shown.clone(),
            reason: e.to_string(),
        })?;
        line.push('\n');
        let write = |source: std::io::Error| StorageError::Write {
            path: shown.clone(),
            source,
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)
            .map_err(write)?;
        file.write_all(line.as_bytes()).map_err(write)?;
        file.sync_all().map_err(write)?;
        drop(file);
        #[cfg(unix)]
        if !existed {
            let parent = match self.path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            std::fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(write)?;
        }
        Ok(())
    }

    /// How big the journal has grown since the last checkpoint, in bytes.
    pub fn journal_len(&self) -> u64 {
        std::fs::metadata(self.journal_path()).map_or(0, |m| m.len())
    }

    /// Write the store atomically, creating parent directories as needed.
    ///
    /// This is the *checkpoint* write (#103): rewriting the whole history
    /// is the cost the journal exists to keep off the mutation path, so
    /// this runs at seed time and whenever the journal has grown past
    /// [`JOURNAL_CHECKPOINT_BYTES`]. On success the journal is truncated:
    /// everything it held is in the checkpoint just renamed into place.
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
        // without cloning the whole store first -- a checkpoint is already
        // the one whole-history write left, without paying for a duplicate
        // of it in memory too. Field order matches `Store`'s, so the bytes
        // on disk are unchanged.
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
        // The checkpoint now holds everything the journal held (#103).
        // Truncate after the rename, never before: a crash in between
        // replays the journal over the new checkpoint, which is
        // idempotent, while truncating first could lose the only copy
        // of a journaled mutation. A missing journal file is fine; any
        // other failure is reported, with the checkpoint already saved.
        let journal = self.journal_path();
        if journal.exists() {
            let shown_j = journal.display().to_string();
            let f = std::fs::File::create(&journal).map_err(|source| StorageError::Write {
                path: shown_j.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| StorageError::Write {
                path: shown_j,
                source,
            })?;
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
    fn a_journal_diff_round_trips_between_stores() {
        // #103: apply(between(a, b), a) == b, and applying it twice more
        // changes nothing (the idempotence checkpoint-truncate relies on).
        let before = sample();
        let mut after = before.clone();
        after.entries[0].description = "rewritten".into();
        after.entries.push(Entry {
            id: "e2".into(),
            project_id: "p1".into(),
            description: "new".into(),
            started_at: 5_000,
            ended_at: 6_000,
            source: EntrySource::QuickAdd,
            note: None,
        });
        after.projects[0].archived = true;
        let record = Journal::between(&before, &after);
        assert_eq!(record.entries.len(), 2, "changed + created entries");
        assert!(record.removed_entries.is_empty());
        assert_eq!(record.projects.len(), 1);
        let mut got = before.clone();
        record.apply_to(&mut got);
        assert_eq!(got, after);
        record.apply_to(&mut got);
        assert_eq!(got, after, "replay is idempotent");
        let removal = Journal::between(&after, &before);
        assert_eq!(removal.removed_entries, ["e2".to_string()]);
        removal.apply_to(&mut got);
        assert_eq!(got, before);
        assert!(Journal::between(&before, &before).is_empty());
    }

    #[test]
    fn appended_journals_replay_over_the_checkpoint() {
        // #103: append must not touch the checkpoint, and a fresh load
        // must rebuild the mutated store from checkpoint + journal.
        let dir = tmpdir("journal-replay");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let store = JsonStore::new(path.clone());
        let before = sample();
        store.save(&before).unwrap();
        let checkpoint = std::fs::read(&path).unwrap();

        let mut after = before.clone();
        after.entries.push(Entry {
            id: "e2".into(),
            project_id: "p1".into(),
            description: "journaled".into(),
            started_at: 5_000,
            ended_at: 6_500,
            source: EntrySource::Manual,
            note: None,
        });
        let record = Journal::between(&before, &after);
        store.append(&record).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            checkpoint,
            "append must not rewrite the checkpoint"
        );
        let mut expected = after.clone();
        expected.version = STORE_VERSION;
        assert_eq!(store.load().unwrap(), expected);

        // A checkpoint folds the journal in and truncates it.
        store.save(&after).unwrap();
        assert_eq!(store.journal_len(), 0);
        assert_eq!(store.load().unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pre_journal_store_loads_identically_and_then_journals() {
        // #103 migration: an existing store.json with no journal file at
        // all loads byte-for-byte the same store as before, and later
        // appends land in a new journal beside it.
        let dir = tmpdir("pre-journal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&{
                let mut s = sample();
                s.version = STORE_VERSION;
                s
            })
            .unwrap(),
        )
        .unwrap();
        let store = JsonStore::new(path);
        assert!(!store.journal_path().exists());
        let loaded = store.load().unwrap();
        let mut expected = sample();
        expected.version = STORE_VERSION;
        assert_eq!(loaded, expected);
        store
            .append(&Journal {
                removed_entries: vec!["e1".into()],
                ..Default::default()
            })
            .unwrap();
        assert!(store.load().unwrap().entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_final_journal_line_is_dropped_but_corruption_is_refused() {
        // #103: a crash mid-append leaves a partial last line with no
        // newline — that mutation was never acknowledged, so load drops
        // it. Corruption anywhere else refuses, like checkpoint damage.
        let dir = tmpdir("journal-torn");
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonStore::new(dir.join("store.json"));
        store.save(&sample()).unwrap();
        let jpath = store.journal_path();
        std::fs::write(&jpath, "{\"entries\":[{\"id\":\"e2\"").unwrap();
        let mut expected = sample();
        expected.version = STORE_VERSION;
        assert_eq!(store.load().unwrap(), expected, "torn tail dropped");
        std::fs::write(&jpath, "not json\n{\"entries\":[]}\n").unwrap();
        assert!(store.load().is_err(), "mid-journal corruption refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_mutation_on_a_multi_year_store_writes_one_small_line() {
        // #103's demonstration, in bytes rather than timing (so it holds
        // on any CI disk): over a generated 20,000-entry store (~5 years
        // at 10/day), one appended entry must leave the multi-megabyte
        // checkpoint byte-identical and add well under a kilobyte of
        // journal, where the old design rewrote the whole file per tap.
        let dir = tmpdir("journal-many-years");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let store = JsonStore::new(path.clone());
        let mut big = Store::default();
        big.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: None,
            archived: false,
        });
        for n in 1..=20_000 {
            let start = 1_700_000_000_000 + n as i64 * 3_600_000;
            big.entries.push(Entry {
                id: format!("e{n}"),
                project_id: "p1".into(),
                description: format!("task {n}"),
                started_at: start,
                ended_at: start + 1_800_000,
                source: EntrySource::Manual,
                note: None,
            });
        }
        store.save(&big).unwrap();
        let checkpoint_bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            checkpoint_bytes > 1_000_000,
            "the workload is genuinely multi-megabyte: {checkpoint_bytes}"
        );
        let checkpoint = std::fs::read(&path).unwrap();

        let mut after = big.clone();
        after.entries.push(Entry {
            id: "e20001".into(),
            project_id: "p1".into(),
            description: "one more tap".into(),
            started_at: 1_800_000_000_000,
            ended_at: 1_800_000_900_000,
            source: EntrySource::QuickAdd,
            note: None,
        });
        store.append(&Journal::between(&big, &after)).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            checkpoint,
            "the checkpoint was not rewritten"
        );
        assert!(
            store.journal_len() < 1_024,
            "one mutation, one small line: {} bytes",
            store.journal_len()
        );
        let loaded = store.load().unwrap();
        assert_eq!(loaded.entries.len(), 20_001);
        assert_eq!(loaded.entries.last().unwrap().id, "e20001");
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
