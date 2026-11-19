/* service/sync/crypto.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! XChaCha20-Poly1305 sealing and opening for sync envelopes (#171).
//!
//! Wire layout:
//! ```text
//!   bytes 0..4   magic ("TTS1")
//!   bytes 4..6   wire_version (big-endian u16, currently 1)
//!   bytes 6..30  random 192-bit nonce (24 bytes)
//!   bytes 30..   XChaCha20-Poly1305(key, nonce, aad = bytes[0..6], plaintext = JSON)
//! ```
//!
//! Versioning follows REQUIREMENTS §2:
//! - Both `wire_version > WIRE_VERSION` (checked before decryption) and
//!   `format_version > ENVELOPE_FORMAT_VERSION` (checked after decryption)
//!   fail loudly with [`EnvelopeError::TooNew`].
//! - Corrupt bytes, truncated frames, or wrong-key/tampered ciphertexts
//!   return [`EnvelopeError::Corrupt`] / [`EnvelopeError::DecryptFailed`];
//!   batch readers log a warning and skip them so last-good state stands.

use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::Deserialize;
use zeroize::Zeroizing;

use super::envelope::{
    ENVELOPE_FORMAT_VERSION, Envelope, NONCE_LEN, WIRE_MAGIC, WIRE_VERSION, is_valid_device_id,
};
use super::key::SyncKey;

const HEADER_LEN: usize = 4 + 2;
const TAG_LEN: usize = 16;
const MIN_FRAME_LEN: usize = HEADER_LEN + NONCE_LEN + TAG_LEN;

/// Why sealing or opening a sync envelope failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error(
        "sync envelope format v{found} was written by a newer TimeTrack (this build reads up to v{supported})"
    )]
    TooNew { found: u32, supported: u32 },
    #[error("could not decrypt sync envelope (wrong key or tampered data)")]
    DecryptFailed,
    #[error("corrupt sync envelope: {0}")]
    Corrupt(String),
    #[error("invalid sync device id '{0}'")]
    InvalidDeviceId(String),
    #[error("could not generate envelope nonce: {0}")]
    RandomnessFailed(String),
}

fn wire_header(version: u16) -> [u8; HEADER_LEN] {
    let ver = version.to_be_bytes();
    [
        WIRE_MAGIC[0],
        WIRE_MAGIC[1],
        WIRE_MAGIC[2],
        WIRE_MAGIC[3],
        ver[0],
        ver[1],
    ]
}

/// Seal `envelope` into an authenticated, encrypted wire frame under `key`.
pub fn seal_envelope(envelope: &Envelope, key: &SyncKey) -> Result<Vec<u8>, EnvelopeError> {
    if !is_valid_device_id(&envelope.device_id) {
        return Err(EnvelopeError::InvalidDeviceId(envelope.device_id.clone()));
    }
    let plaintext = Zeroizing::new(
        serde_json::to_vec(envelope).map_err(|e| EnvelopeError::Corrupt(e.to_string()))?,
    );
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|e| EnvelopeError::RandomnessFailed(e.to_string()))?;

    let header = wire_header(WIRE_VERSION);
    let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
    let nonce = XNonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_slice(),
                aad: &header,
            },
        )
        .map_err(|_| EnvelopeError::Corrupt("AEAD encryption failed".into()))?;

    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

#[derive(Deserialize)]
struct VersionProbe {
    format_version: u32,
}

/// Open and authenticate an envelope frame under `key`.
pub fn open_envelope(bytes: &[u8], key: &SyncKey) -> Result<Envelope, EnvelopeError> {
    if bytes.len() < MIN_FRAME_LEN {
        return Err(EnvelopeError::Corrupt(
            "envelope shorter than minimum header + nonce + tag".into(),
        ));
    }
    if bytes[..4] != WIRE_MAGIC {
        return Err(EnvelopeError::Corrupt(
            "missing TTS1 envelope header".into(),
        ));
    }
    let wire_ver = u16::from_be_bytes([bytes[4], bytes[5]]);
    if wire_ver == 0 {
        return Err(EnvelopeError::Corrupt(
            "envelope wire version 0 is invalid".into(),
        ));
    }
    if wire_ver > WIRE_VERSION {
        return Err(EnvelopeError::TooNew {
            found: u32::from(wire_ver),
            supported: u32::from(WIRE_VERSION),
        });
    }

    let header = &bytes[..HEADER_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&bytes[HEADER_LEN..HEADER_LEN + NONCE_LEN]);
    let ciphertext = &bytes[HEADER_LEN + NONCE_LEN..];

    let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &XNonce::from(nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad: header,
                },
            )
            .map_err(|_| EnvelopeError::DecryptFailed)?,
    );

    let probe: VersionProbe = serde_json::from_slice(plaintext.as_slice())
        .map_err(|e| EnvelopeError::Corrupt(e.to_string()))?;
    if probe.format_version == 0 {
        return Err(EnvelopeError::Corrupt(
            "envelope format_version 0 is invalid".into(),
        ));
    }
    if probe.format_version > ENVELOPE_FORMAT_VERSION {
        return Err(EnvelopeError::TooNew {
            found: probe.format_version,
            supported: ENVELOPE_FORMAT_VERSION,
        });
    }

    let envelope: Envelope = serde_json::from_slice(plaintext.as_slice())
        .map_err(|e| EnvelopeError::Corrupt(e.to_string()))?;
    if !is_valid_device_id(&envelope.device_id) {
        return Err(EnvelopeError::InvalidDeviceId(envelope.device_id));
    }
    Ok(envelope)
}

/// Result of opening a batch of named peer envelopes under the §2 policy.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenedBatch {
    pub envelopes: Vec<Envelope>,
    pub warnings: Vec<String>,
}

/// Open a collection of `(filename, bytes)` peer envelopes under the §2 rule:
/// - Any newer-format envelope (`TooNew`) fails the whole batch loudly.
/// - Corrupt frames, decryption failures, or mismatched filenames emit a
///   warning and are skipped so previously merged state stands.
#[cfg(test)]
pub fn open_envelope_batch<'a, I>(items: I, key: &SyncKey) -> Result<OpenedBatch, EnvelopeError>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut out = OpenedBatch::default();
    for (name, bytes) in items {
        match open_envelope(bytes, key) {
            Ok(env) => {
                if env.device_id != name {
                    out.warnings.push(format!(
                        "ignored peer envelope '{name}': inner device_id '{}' does not match file name",
                        env.device_id
                    ));
                    continue;
                }
                out.envelopes.push(env);
            }
            Err(err @ EnvelopeError::TooNew { .. }) => return Err(err),
            Err(err) => {
                out.warnings
                    .push(format!("ignored unreadable peer envelope '{name}': {err}"));
            }
        }
    }
    Ok(out)
}
