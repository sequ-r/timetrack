/* service/sync/envelope.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Sync envelope schema and random-hex file naming (ADR 0020, #171).
//!
//! Each paired device writes one envelope file named strictly by its
//! random 32-hex `device_id`. The folder host sees only opaque blobs,
//! sizes, and mtimes — no plaintext record data, locators, usernames, or
//! extensions in the file name.

use serde::{Deserialize, Serialize};
use timetrack_core::EntrySource;

use super::crypto::EnvelopeError;

/// Inner JSON payload schema version this build writes and understands.
/// Bumped only when the decrypted envelope shape breaks compatibility;
/// envelopes claiming a higher version are refused loudly (§2 rule).
pub const ENVELOPE_FORMAT_VERSION: u32 = 1;

/// Magic prefix identifying a TimeTrack sync envelope wire frame.
pub const WIRE_MAGIC: [u8; 4] = *b"TTS1";

/// Wire framing revision (header + AEAD layout). Newer wire revisions are
/// refused before attempting decryption.
pub const WIRE_VERSION: u16 = 1;

/// XChaCha20-Poly1305 extended nonce length, in bytes.
pub const NONCE_LEN: usize = 24;

/// A single device's full-state snapshot published to the sync folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub format_version: u32,
    pub device_id: String,
    pub published_at: i64,
    pub merged_through: i64,
    #[serde(default)]
    pub compacted_through: i64,
    #[serde(default)]
    pub projects: Vec<SyncedProject>,
    #[serde(default)]
    pub entries: Vec<SyncedEntry>,
    #[serde(default)]
    pub sources: Vec<SyncedSource>,
    #[serde(default)]
    pub tombstones: Vec<SyncedTombstone>,
}

/// A project record inside a sync envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedProject {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub colour: Option<u32>,
    #[serde(default)]
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// An entry record inside a sync envelope.
///
/// Confirmed `External` entries optionally carry `(external_source_id,
/// external_uid, external_started_at)` so peers dedup suggestions across
/// devices (#174) without syncing per-device suggestion caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedEntry {
    pub id: String,
    pub project_id: String,
    pub description: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub source: EntrySource,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_started_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// An external-source configuration inside a sync envelope.
///
/// Deliberately carries no `cred_ref` and no `cred_username`: secrets
/// stay per-device in each machine's OS credential store (#149, ADR 0020).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedSource {
    pub id: String,
    pub kind: String,
    pub locator: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub label: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub device_id: String,
}

/// A deletion marker inside a sync envelope (`"entry"`, `"project"`, or `"source"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedTombstone {
    pub id: String,
    pub kind: String,
    pub deleted_at: i64,
    pub device_id: String,
}

/// Whether `id` has the exact 32-char lowercase hex shape minted by
/// `timetrack-store` (`lower(hex(randomblob(16)))`).
pub fn is_valid_device_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Envelope filename for a device: strictly the random-hex `device_id`,
/// with no extension or user-identifying metadata.
pub fn envelope_filename(device_id: &str) -> Result<String, EnvelopeError> {
    if !is_valid_device_id(device_id) {
        return Err(EnvelopeError::InvalidDeviceId(device_id.to_string()));
    }
    Ok(device_id.to_string())
}

/// Extract the `device_id` if a directory entry name is a valid
/// envelope filename, ignoring temporary files, hidden files, or notes.
pub fn parse_envelope_filename(name: &str) -> Option<&str> {
    if is_valid_device_id(name) {
        Some(name)
    } else {
        None
    }
}
