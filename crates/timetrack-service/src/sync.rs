/* service/sync.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! One-person folder sync (ADR 0020).
//!
//! Encrypted per-device state snapshots written to and read from a
//! user-held replicated folder (Syncthing, Nextcloud, USB, NFS), using
//! XChaCha20-Poly1305 and a 256-bit sync key stored in the OS
//! credential store (#149).

// Wired by the publish/pull/merge flow (#173); tested directly until then.
#[allow(dead_code)]
pub mod crypto;
#[allow(dead_code)]
pub mod envelope;
#[allow(dead_code)]
pub mod key;

#[cfg(test)]
mod tests {
    use super::crypto::{EnvelopeError, open_envelope, open_envelope_batch, seal_envelope};
    use super::envelope::{
        ENVELOPE_FORMAT_VERSION, Envelope, SyncedEntry, SyncedProject, SyncedSource,
        SyncedTombstone, WIRE_VERSION, envelope_filename, is_valid_device_id,
        parse_envelope_filename,
    };
    use super::key::{
        SYNC_KEY_REF, SyncKey, SyncKeyError, delete_sync_key, generate_and_store_sync_key,
        load_sync_key, rotate_sync_key, store_sync_key,
    };
    use crate::credentials::{CredentialStore, FakeCredentialStore};
    use chacha20poly1305::{
        Key, XChaCha20Poly1305, XNonce,
        aead::{Aead, KeyInit, Payload},
    };
    use timetrack_core::EntrySource;
    use timetrack_store::SqliteStore;

    const DEV_A: &str = "0123456789abcdef0123456789abcdef";
    const DEV_B: &str = "fedcba9876543210fedcba9876543210";

    fn sample_envelope() -> Envelope {
        Envelope {
            format_version: ENVELOPE_FORMAT_VERSION,
            device_id: DEV_A.into(),
            published_at: 1_700_000_010_000,
            merged_through: 1_700_000_005_000,
            compacted_through: 1_700_000_000_000,
            projects: vec![SyncedProject {
                id: "11111111-1111-4111-8111-111111111111".into(),
                name: "Secret Client Project Omega".into(),
                colour: Some(0x2ea043),
                archived: false,
                created_at: 1_700_000_001_000,
                updated_at: 1_700_000_002_000,
                device_id: DEV_A.into(),
            }],
            entries: vec![SyncedEntry {
                id: "22222222-2222-4222-8222-222222222222".into(),
                project_id: "11111111-1111-4111-8111-111111111111".into(),
                description: "Confidential architecture review".into(),
                started_at: 1_700_000_001_000,
                ended_at: 1_700_000_061_000,
                source: EntrySource::External,
                note: Some("Discussed with alice@example.com".into()),
                external_source_id: Some("33333333-3333-4333-8333-333333333333".into()),
                external_uid: Some("caldav-uid-98765".into()),
                external_started_at: Some(1_700_000_001_000),
                created_at: 1_700_000_003_000,
                updated_at: 1_700_000_004_000,
                device_id: DEV_A.into(),
            }],
            sources: vec![SyncedSource {
                id: "33333333-3333-4333-8333-333333333333".into(),
                kind: "caldav".into(),
                locator: "https://alice@calendar.private.example/dav/work/".into(),
                project_id: Some("11111111-1111-4111-8111-111111111111".into()),
                label: "AliceWorkCal".into(),
                created_at: 1_700_000_000_500,
                updated_at: 1_700_000_000_500,
                device_id: DEV_A.into(),
            }],
            tombstones: vec![SyncedTombstone {
                id: "44444444-4444-4444-8444-444444444444".into(),
                kind: "entry".into(),
                deleted_at: 1_700_000_009_000,
                device_id: DEV_A.into(),
            }],
        }
    }

    #[test]
    fn seal_and_open_round_trips_with_fresh_nonces() {
        let key = SyncKey::generate().unwrap();
        let env = sample_envelope();
        let bytes1 = seal_envelope(&env, &key).unwrap();
        let bytes2 = seal_envelope(&env, &key).unwrap();
        assert_ne!(bytes1, bytes2, "each seal must mint a fresh random nonce");
        assert_eq!(open_envelope(&bytes1, &key).unwrap(), env);
        assert_eq!(open_envelope(&bytes2, &key).unwrap(), env);
    }

    #[test]
    fn tampered_ciphertext_nonce_aad_and_wrong_key_are_refused() {
        let key = SyncKey::generate().unwrap();
        let wrong_key = SyncKey::generate().unwrap();
        let env = sample_envelope();
        let sealed = seal_envelope(&env, &key).unwrap();

        // Wrong key refuses.
        assert_eq!(
            open_envelope(&sealed, &wrong_key),
            Err(EnvelopeError::DecryptFailed)
        );

        // Tamper one byte in ciphertext/tag.
        let mut tampered_body = sealed.clone();
        let last = tampered_body.len() - 1;
        tampered_body[last] ^= 0x01;
        assert_eq!(
            open_envelope(&tampered_body, &key),
            Err(EnvelopeError::DecryptFailed)
        );

        // Tamper one byte in nonce.
        let mut tampered_nonce = sealed.clone();
        tampered_nonce[10] ^= 0x80;
        assert_eq!(
            open_envelope(&tampered_nonce, &key),
            Err(EnvelopeError::DecryptFailed)
        );

        // Tamper header wire version -> refused as TooNew before decrypt if bumped up,
        // or DecryptFailed if flipped to another accepted wire version (here 0 is corrupt).
        let mut zero_ver = sealed.clone();
        zero_ver[5] = 0;
        assert!(matches!(
            open_envelope(&zero_ver, &key),
            Err(EnvelopeError::Corrupt(_))
        ));

        // Tamper inner AAD check by sealing with modified AAD under same key.
        let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
        let nonce = [7u8; 24];
        let wrong_aad_ct = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: br#"{"format_version":1}"#.as_slice(),
                    aad: b"TTS1\x00\x02",
                },
            )
            .unwrap();
        let mut forged = Vec::new();
        forged.extend_from_slice(b"TTS1\x00\x01");
        forged.extend_from_slice(&nonce);
        forged.extend_from_slice(&wrong_aad_ct);
        assert_eq!(
            open_envelope(&forged, &key),
            Err(EnvelopeError::DecryptFailed)
        );
    }

    #[test]
    fn newer_wire_and_inner_format_versions_refuse_loudly() {
        let key = SyncKey::generate().unwrap();
        let env = sample_envelope();
        let mut newer_wire = seal_envelope(&env, &key).unwrap();
        let bumped_wire = WIRE_VERSION + 1;
        newer_wire[4..6].copy_from_slice(&bumped_wire.to_be_bytes());
        let err = open_envelope(&newer_wire, &key).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::TooNew {
                found: u32::from(bumped_wire),
                supported: u32::from(WIRE_VERSION),
            }
        );
        assert!(err.to_string().contains("newer TimeTrack"));

        // Inner JSON format_version > ENVELOPE_FORMAT_VERSION (even if encrypted
        // with the right key and containing unknown future fields).
        let future_json = format!(
            r#"{{"format_version":{},"device_id":"{DEV_A}","future_field":123}}"#,
            ENVELOPE_FORMAT_VERSION + 1
        );
        let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
        let nonce = [9u8; 24];
        let header = [b'T', b'T', b'S', b'1', 0, 1];
        let ct = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: future_json.as_bytes(),
                    aad: &header,
                },
            )
            .unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&nonce);
        frame.extend_from_slice(&ct);
        let err = open_envelope(&frame, &key).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::TooNew {
                found: ENVELOPE_FORMAT_VERSION + 1,
                supported: ENVELOPE_FORMAT_VERSION,
            }
        );
        assert!(err.to_string().contains("newer TimeTrack"));
    }

    #[test]
    fn batch_reader_skips_corrupt_with_warning_and_fails_on_too_new() {
        let key = SyncKey::generate().unwrap();
        let good_env = sample_envelope();
        let good_bytes = seal_envelope(&good_env, &key).unwrap();

        let mut env_b = sample_envelope();
        env_b.device_id = DEV_B.into();
        let mismatched_bytes = seal_envelope(&env_b, &key).unwrap();

        let corrupt_bytes = b"not-an-envelope".to_vec();
        let mut tampered_bytes = good_bytes.clone();
        let last = tampered_bytes.len() - 1;
        tampered_bytes[last] ^= 0xff;

        let batch = open_envelope_batch(
            [
                (DEV_A, good_bytes.as_slice()),
                ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", corrupt_bytes.as_slice()),
                (
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    tampered_bytes.as_slice(),
                ),
                (DEV_A, mismatched_bytes.as_slice()),
            ],
            &key,
        )
        .unwrap();

        assert_eq!(batch.envelopes, vec![good_env]);
        assert_eq!(batch.warnings.len(), 3);
        assert!(batch.warnings[0].contains("corrupt sync envelope"));
        assert!(batch.warnings[1].contains("wrong key or tampered"));
        assert!(batch.warnings[2].contains("does not match file name"));

        // Any TooNew envelope in the batch fails the whole batch loudly (§2 rule).
        let mut future_bytes = good_bytes.clone();
        let bumped = WIRE_VERSION + 1;
        future_bytes[4..6].copy_from_slice(&bumped.to_be_bytes());
        let err = open_envelope_batch(
            [
                (DEV_A, good_bytes.as_slice()),
                (DEV_B, future_bytes.as_slice()),
            ],
            &key,
        )
        .unwrap_err();
        assert!(matches!(err, EnvelopeError::TooNew { .. }));
    }

    #[test]
    fn no_plaintext_record_data_locators_or_usernames_in_filename_or_bytes() {
        let key = SyncKey::generate().unwrap();
        let env = sample_envelope();
        let filename = envelope_filename(&env.device_id).unwrap();
        let sealed = seal_envelope(&env, &key).unwrap();

        assert!(is_valid_device_id(&filename));
        assert_eq!(filename, DEV_A);
        assert_eq!(parse_envelope_filename(&filename), Some(DEV_A));
        assert_eq!(parse_envelope_filename("notes.txt"), None);
        assert_eq!(parse_envelope_filename(&format!("{DEV_A}.enc")), None);
        assert_eq!(parse_envelope_filename(&DEV_A.to_uppercase()), None);

        for sensitive in [
            "Secret Client Project Omega",
            "Confidential architecture review",
            "alice@example.com",
            "calendar.private.example",
            "AliceWorkCal",
            "caldav-uid-98765",
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "33333333-3333-4333-8333-333333333333",
            "caldav",
            DEV_A,
        ] {
            if sensitive != DEV_A {
                assert!(
                    !filename.contains(sensitive),
                    "filename leaked {sensitive:?}"
                );
            }
            assert!(
                !sealed
                    .windows(sensitive.len())
                    .any(|w| w == sensitive.as_bytes()),
                "sealed ciphertext leaked {sensitive:?}"
            );
        }
    }

    #[test]
    fn pairing_code_base32_round_trips_and_rejects_bad_inputs() {
        let key = SyncKey::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0,
            0xd0, 0xe0, 0xf0, 0xff,
        ]);
        let code = key.to_pairing_code();
        assert_eq!(code.replace('-', "").len(), 52);

        let decoded = SyncKey::from_pairing_code(&code).unwrap();
        assert_eq!(decoded.as_bytes(), key.as_bytes());

        // Case-insensitive, whitespace/hyphen tolerant.
        let messy = format!("  {} \n", code.to_lowercase().replace('-', " "));
        let decoded_messy = SyncKey::from_pairing_code(&messy).unwrap();
        assert_eq!(decoded_messy.as_bytes(), key.as_bytes());

        // Redacted Debug output never prints key bytes or pairing code.
        assert_eq!(format!("{key:?}"), "SyncKey(redacted)");
        assert!(!format!("{key:?}").contains(&*code));

        // Bad lengths / chars / non-zero padding bits are refused without echoing input.
        let bad_input = "ZZZZ-THIS-IS-NOT-A-VALID-KEY-AT-ALL";
        let err = SyncKey::from_pairing_code(bad_input).unwrap_err();
        assert_eq!(err, SyncKeyError::InvalidPairingCode);
        assert!(!err.to_string().contains("ZZZZ"));
        assert!(!format!("{err:?}").contains("ZZZZ"));

        assert_eq!(
            SyncKey::from_pairing_code("").unwrap_err(),
            SyncKeyError::InvalidPairingCode
        );
        // In 256 bits -> 52 * 5 = 260 bits: the 52nd Base32 char carries 1 key
        // bit (bit 4) and 4 zero padding bits (bits 0..4). Flipping bit 0 of
        // the last 5-bit symbol guarantees a non-zero padding bit.
        let mut chars: Vec<char> = code.chars().collect();
        let last_char = *chars.last().unwrap();
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let pos = alphabet.find(last_char).unwrap();
        let flipped_char = alphabet.as_bytes()[pos ^ 1] as char;
        *chars.last_mut().unwrap() = flipped_char;
        let flipped: String = chars.into_iter().collect();
        assert_eq!(
            SyncKey::from_pairing_code(&flipped).unwrap_err(),
            SyncKeyError::InvalidPairingCode
        );
    }

    #[test]
    fn key_lifecycle_on_fake_credential_store_and_never_in_sqlite_or_export() {
        let creds = FakeCredentialStore::new();
        assert!(load_sync_key(&creds).unwrap().is_none());

        let generated = generate_and_store_sync_key(&creds).unwrap();
        let loaded = load_sync_key(&creds).unwrap().expect("key stored");
        assert_eq!(loaded.as_bytes(), generated.as_bytes());

        // Store an imported pairing code.
        let imported = SyncKey::from_bytes([0x42; 32]);
        store_sync_key(&creds, &imported).unwrap();
        assert_eq!(
            load_sync_key(&creds).unwrap().unwrap().as_bytes(),
            imported.as_bytes()
        );

        // Rotate produces a new key and overwrites the stored entry.
        let rotated = rotate_sync_key(&creds).unwrap();
        assert_ne!(rotated.as_bytes(), imported.as_bytes());
        assert_eq!(
            load_sync_key(&creds).unwrap().unwrap().as_bytes(),
            rotated.as_bytes()
        );

        // Corrupt stored string fails cleanly without leaking stored bytes.
        creds.set(SYNC_KEY_REF, "corrupt-secret-material").unwrap();
        let err = load_sync_key(&creds).unwrap_err();
        assert_eq!(err, SyncKeyError::CorruptStoredKey);
        assert!(!err.to_string().contains("corrupt-secret-material"));

        // Unavailable OS store surfaces cleanly.
        creds.fail_with("keyring locked");
        assert!(matches!(
            load_sync_key(&creds),
            Err(SyncKeyError::Unavailable(_))
        ));
        assert!(matches!(
            rotate_sync_key(&creds),
            Err(SyncKeyError::Unavailable(_))
        ));
        assert!(matches!(
            delete_sync_key(&creds),
            Err(SyncKeyError::Unavailable(_))
        ));
        creds.heal();

        // Delete removes the entry and is idempotent.
        delete_sync_key(&creds).unwrap();
        assert!(load_sync_key(&creds).unwrap().is_none());
        delete_sync_key(&creds).unwrap();

        // Confirm key material never lands in SQLite or CSV exports.
        let dir = std::env::temp_dir().join(format!("tt-sync-key-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("store.db");
        let store = SqliteStore::open_file(&db_path).unwrap();
        let raw_db = std::fs::read(&db_path).unwrap();
        let code = rotated.to_pairing_code();
        assert!(!raw_db.windows(code.len()).any(|w| w == code.as_bytes()));
        assert!(
            !raw_db
                .windows(rotated.as_bytes().len())
                .any(|w| w == rotated.as_bytes())
        );

        let loaded_store = store.load().unwrap();
        let csv = timetrack_core::export_csv(
            &loaded_store,
            timetrack_core::ExportScope::All,
            0,
            &timetrack_core::Tz::utc(),
        );
        assert!(!csv.contains(&*code));
        assert!(!csv.contains(SYNC_KEY_REF));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
