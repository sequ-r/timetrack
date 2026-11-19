/* service/sync/key.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! 256-bit sync key generation, Base32 pairing code encoding, and OS
//! credential store lifecycle on the #149 `CredentialStore` seam.
//!
//! Key material never touches SQLite, logs, exports, or argv, and
//! [`SyncKey`] zeroizes its bytes on drop and redacts its `Debug` output.

use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::credentials::{CredentialError, CredentialStore};

/// Account/reference key under `org.sequ.timetrack` in the OS store.
/// Namespaced away from `caldav/<source_id>` entries (#149).
pub const SYNC_KEY_REF: &str = "sync/key";

/// Number of raw bytes in an XChaCha20-Poly1305 sync key.
pub const SYNC_KEY_LEN: usize = 32;

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const PAIRING_CODE_UNGROUPED_LEN: usize = 52;

/// A 256-bit symmetric sync key, zeroized on drop and never printable
/// through `Debug` or `Display`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SyncKey([u8; SYNC_KEY_LEN]);

impl std::fmt::Debug for SyncKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncKey(redacted)")
    }
}

/// Why a sync-key operation failed. Never carries key bytes or pairing
/// code inputs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncKeyError {
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Store(String),
    #[error("invalid sync pairing code (expected 52 Base32 characters)")]
    InvalidPairingCode,
    #[error("stored sync key in OS credential store is corrupt; re-pair or rotate the sync key")]
    CorruptStoredKey,
    #[error("could not read OS randomness for sync key: {0}")]
    RandomnessFailed(String),
}

impl From<CredentialError> for SyncKeyError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::Unavailable(msg) => SyncKeyError::Unavailable(msg),
            CredentialError::Other(msg) => SyncKeyError::Store(msg),
        }
    }
}

impl SyncKey {
    /// Generate a fresh random 256-bit sync key using OS randomness.
    pub fn generate() -> Result<Self, SyncKeyError> {
        let mut bytes = [0u8; SYNC_KEY_LEN];
        getrandom::fill(&mut bytes).map_err(|e| SyncKeyError::RandomnessFailed(e.to_string()))?;
        Ok(SyncKey(bytes))
    }

    /// Wrap raw 32-byte key material.
    pub fn from_bytes(bytes: [u8; SYNC_KEY_LEN]) -> Self {
        SyncKey(bytes)
    }

    /// Borrow the raw key bytes for AEAD seal/open.
    pub(crate) fn as_bytes(&self) -> &[u8; SYNC_KEY_LEN] {
        &self.0
    }

    /// Encode this key as a grouped RFC 4648 Base32 pairing code
    /// (`XXXX-XXXX-...`, 13 groups of 4 uppercase Base32 characters).
    pub fn to_pairing_code(&self) -> Zeroizing<String> {
        let mut ungrouped = Zeroizing::new(Vec::with_capacity(PAIRING_CODE_UNGROUPED_LEN));
        let mut acc: u16 = 0;
        let mut bits: u8 = 0;
        for &byte in &self.0 {
            acc = (acc << 8) | u16::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                let idx = ((acc >> bits) & 0x1F) as usize;
                ungrouped.push(BASE32_ALPHABET[idx]);
            }
        }
        if bits > 0 {
            let idx = ((acc << (5 - bits)) & 0x1F) as usize;
            ungrouped.push(BASE32_ALPHABET[idx]);
        }
        let mut grouped = Zeroizing::new(String::with_capacity(PAIRING_CODE_UNGROUPED_LEN + 12));
        for (i, &ch) in ungrouped.iter().enumerate() {
            if i > 0 && i % 4 == 0 {
                grouped.push('-');
            }
            grouped.push(ch as char);
        }
        grouped
    }

    /// Decode a pairing code entered out-of-band (ignoring ASCII whitespace
    /// and hyphens, case-insensitive). Error messages carry no input bytes.
    pub fn from_pairing_code(code: &str) -> Result<Self, SyncKeyError> {
        let mut cleaned = Zeroizing::new(Vec::with_capacity(PAIRING_CODE_UNGROUPED_LEN));
        for b in code.bytes() {
            if b.is_ascii_whitespace() || b == b'-' {
                continue;
            }
            let val = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a',
                b'2'..=b'7' => b - b'2' + 26,
                _ => return Err(SyncKeyError::InvalidPairingCode),
            };
            cleaned.push(val);
        }
        if cleaned.len() != PAIRING_CODE_UNGROUPED_LEN {
            return Err(SyncKeyError::InvalidPairingCode);
        }
        let mut out = [0u8; SYNC_KEY_LEN];
        let mut out_idx = 0usize;
        let mut acc: u16 = 0;
        let mut bits: u8 = 0;
        for &val in cleaned.iter() {
            acc = (acc << 5) | u16::from(val);
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out[out_idx] = ((acc >> bits) & 0xFF) as u8;
                out_idx += 1;
            }
        }
        // 52 * 5 = 260 bits = 256 key bits + 4 zero padding bits.
        // Reject non-canonical trailing padding bits.
        if out_idx != SYNC_KEY_LEN || (acc & ((1 << bits) - 1)) != 0 {
            out.zeroize();
            return Err(SyncKeyError::InvalidPairingCode);
        }
        Ok(SyncKey(out))
    }
}

/// Generate a fresh sync key and store it in the OS credential store.
pub fn generate_and_store_sync_key(store: &dyn CredentialStore) -> Result<SyncKey, SyncKeyError> {
    let key = SyncKey::generate()?;
    store_sync_key(store, &key)?;
    Ok(key)
}

/// Persist an existing sync key (for instance, entered during pairing)
/// into the OS credential store under [`SYNC_KEY_REF`].
pub fn store_sync_key(store: &dyn CredentialStore, key: &SyncKey) -> Result<(), SyncKeyError> {
    let encoded = key.to_pairing_code();
    store.set(SYNC_KEY_REF, &encoded)?;
    Ok(())
}

/// Load the sync key from the OS credential store, if one is stored.
pub fn load_sync_key(store: &dyn CredentialStore) -> Result<Option<SyncKey>, SyncKeyError> {
    let Some(raw) = store.get(SYNC_KEY_REF)? else {
        return Ok(None);
    };
    let secret = Zeroizing::new(raw);
    let key = SyncKey::from_pairing_code(&secret).map_err(|_| SyncKeyError::CorruptStoredKey)?;
    Ok(Some(key))
}

/// Generate a new sync key and overwrite the stored entry. Only writes
/// after random generation succeeds, so a generation error leaves the old
/// key intact.
pub fn rotate_sync_key(store: &dyn CredentialStore) -> Result<SyncKey, SyncKeyError> {
    let fresh = SyncKey::generate()?;
    store_sync_key(store, &fresh)?;
    Ok(fresh)
}

/// Delete the sync key from the OS credential store (idempotent).
pub fn delete_sync_key(store: &dyn CredentialStore) -> Result<(), SyncKeyError> {
    store.delete(SYNC_KEY_REF)?;
    Ok(())
}
