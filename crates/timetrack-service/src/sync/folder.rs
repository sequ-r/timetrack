/* service/sync/folder.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Per-device envelope files inside the user-held sync folder (ADR 0020, #173).
//!
//! Each device writes only its own `<device_id>` file via sibling temp-write +
//! `sync_all` + atomic rename, and reads every other 32-hex device file.
//! Ignoring non-envelope files (Syncthing `.stfolder`, temp files, `.DS_Store`)
//! keeps folder-sync neighbours harmless while still refusing newer envelope
//! format versions loudly (§2) and turning corrupt/unreadable peer files into
//! warnings so last-good state stands.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::crypto::{self, EnvelopeError};
use super::envelope::{self, Envelope};
use super::key::SyncKey;

#[derive(Debug, Error)]
pub enum FolderError {
    #[error("sync path '{0}' is not a directory")]
    NotADirectory(PathBuf),
    #[error(
        "peer envelope '{file}' uses format v{found}, newer than this build supports (v{supported}); upgrade TimeTrack"
    )]
    TooNew {
        file: String,
        found: u32,
        supported: u32,
    },
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("sync folder I/O failed for '{}': {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Outcome of reading every peer file from the sync folder.
#[derive(Debug, Default)]
pub struct PeerReadResult {
    pub envelopes: Vec<Envelope>,
    pub total_peer_files: usize,
    pub decrypt_failed_count: usize,
    pub warnings: Vec<String>,
}

/// Create `path` if absent (used on `sync enable`), refusing non-directories.
pub fn ensure_sync_folder(path: &Path) -> Result<PathBuf, FolderError> {
    if path.as_os_str().is_empty() {
        return Err(FolderError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sync folder path must not be empty",
            ),
        });
    }
    if path.exists() {
        if !path.is_dir() {
            return Err(FolderError::NotADirectory(path.to_path_buf()));
        }
        return Ok(path.to_path_buf());
    }
    std::fs::create_dir_all(path).map_err(|source| FolderError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !path.is_dir() {
        return Err(FolderError::NotADirectory(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

fn check_existing_sync_folder(folder: &Path) -> Result<(), FolderError> {
    match std::fs::metadata(folder) {
        Ok(meta) => {
            if !meta.is_dir() {
                Err(FolderError::NotADirectory(folder.to_path_buf()))
            } else {
                Ok(())
            }
        }
        Err(source) => Err(FolderError::Io {
            path: folder.to_path_buf(),
            source,
        }),
    }
}

/// Seal `envelope` with `key` and write it atomically to `<folder>/<device_id>`.
pub fn publish_envelope(
    folder: &Path,
    envelope: &Envelope,
    key: &SyncKey,
) -> Result<PathBuf, FolderError> {
    check_existing_sync_folder(folder)?;
    let filename = envelope::envelope_filename(&envelope.device_id)?;
    let target = folder.join(&filename);
    let ciphertext = crypto::seal_envelope(envelope, key)?;

    let mut rand_bytes = [0u8; 8];
    getrandom::fill(&mut rand_bytes).map_err(|e| EnvelopeError::RandomnessFailed(e.to_string()))?;
    let mut suffix = String::with_capacity(16);
    for b in rand_bytes {
        suffix.push_str(&format!("{b:02x}"));
    }
    let tmp_path = folder.join(format!(".tmp.{suffix}"));

    let write_result = (|| -> Result<(), FolderError> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path).map_err(|source| FolderError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        file.write_all(&ciphertext)
            .map_err(|source| FolderError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| FolderError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = std::fs::Permissions::from_mode(0o600);
            file.set_permissions(perms)
                .map_err(|source| FolderError::Io {
                    path: tmp_path.clone(),
                    source,
                })?;
        }
        drop(file);
        std::fs::rename(&tmp_path, &target).map_err(|source| FolderError::Io {
            path: target.clone(),
            source,
        })?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(target)
}

/// Read and decrypt every peer envelope in `folder`, skipping `self_device_id`.
///
/// Newer format versions (`TooNew`) fail loudly per §2; corrupt or undecryptable
/// peer files produce warnings while allowing surviving readable peers to merge.
pub fn read_peer_envelopes(
    folder: &Path,
    self_device_id: &str,
    key: &SyncKey,
) -> Result<PeerReadResult, FolderError> {
    check_existing_sync_folder(folder)?;
    let entries = std::fs::read_dir(folder).map_err(|source| FolderError::Io {
        path: folder.to_path_buf(),
        source,
    })?;

    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| FolderError::Io {
            path: folder.to_path_buf(),
            source,
        })?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(dev_id) = envelope::parse_envelope_filename(&name) else {
            continue;
        };
        if dev_id == self_device_id {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        candidates.push((name, path));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = PeerReadResult::default();
    for (name, path) in candidates {
        out.total_peer_files += 1;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                out.warnings
                    .push(format!("could not read peer envelope {name}: {err}"));
                continue;
            }
        };
        match crypto::open_envelope(&bytes, key) {
            Ok(env) => {
                if env.device_id != name {
                    out.warnings.push(format!(
                        "ignored peer file {name}: inner device_id '{}' does not match filename",
                        env.device_id
                    ));
                    continue;
                }
                out.envelopes.push(env);
            }
            Err(EnvelopeError::TooNew { found, supported }) => {
                return Err(FolderError::TooNew {
                    file: name,
                    found,
                    supported,
                });
            }
            Err(EnvelopeError::DecryptFailed) => {
                out.decrypt_failed_count += 1;
                out.warnings.push(format!(
                    "could not decrypt peer envelope {name} (wrong sync key or corrupt file)"
                ));
            }
            Err(err) => {
                out.warnings
                    .push(format!("ignored corrupt peer envelope {name}: {err}"));
            }
        }
    }
    Ok(out)
}

/// Delete this device's own envelope file from `folder`. Returns `true` if a
/// file existed and was deleted, `false` if already absent.
pub fn delete_device_file(folder: &Path, device_id: &str) -> Result<bool, FolderError> {
    let filename = envelope::envelope_filename(device_id)?;
    let target = folder.join(filename);
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(FolderError::Io {
            path: target,
            source,
        }),
    }
}
