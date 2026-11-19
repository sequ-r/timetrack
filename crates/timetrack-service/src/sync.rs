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

pub mod crypto;
pub mod envelope;
pub mod folder;
pub mod key;
pub mod merge;

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

    // --- merge engine + end-to-end two-store sync (#173) ---

    fn temp_root(_tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let dir = std::env::temp_dir().join(format!(
            "tt-s-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct TestNode {
        svc: std::sync::Arc<crate::interface::EntryService>,
        client: timetrack_proto::socket::SocketClient,
        clock: std::sync::Arc<std::sync::atomic::AtomicI64>,
        creds: std::sync::Arc<FakeCredentialStore>,
    }

    fn spawn_test_node(dir: &std::path::Path, name: &str, start_ms: i64) -> TestNode {
        use std::sync::{
            Arc,
            atomic::{AtomicI64, AtomicU64, Ordering},
        };
        static SOCK_SEQ: AtomicU64 = AtomicU64::new(1);
        let node_dir = dir.join(name);
        std::fs::create_dir_all(&node_dir).unwrap();
        let store = SqliteStore::open_at(&node_dir.join("store.json")).unwrap();
        let creds = Arc::new(FakeCredentialStore::new());
        let clock = Arc::new(AtomicI64::new(start_ms));
        let c = Arc::clone(&clock);
        let svc = Arc::new(
            crate::interface::EntryService::new_with_credentials_and_clock(
                store,
                creds.clone(),
                Arc::new(move || c.load(Ordering::Relaxed)),
            )
            .unwrap(),
        );
        // Distinct socket filename per node so Windows named pipes (`pipe_name_of`
        // uses only `file_name()`) never collide across nodes or parallel tests.
        let sock = node_dir.join(format!(
            "s-{}-{}.sock",
            std::process::id(),
            SOCK_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        crate::socket::serve_at(Arc::clone(&svc), sock.clone()).unwrap();
        let client = timetrack_proto::socket::SocketClient::new(sock);
        TestNode {
            svc,
            client,
            clock,
            creds,
        }
    }

    #[test]
    fn merge_lww_tiebreak_tombstone_resurrection_and_source_collision() {
        use super::merge::merge_snapshots;
        use timetrack_store::{ExternalSource, ExternalSourceRow, SyncConfig, SyncRows, Tombstone};

        let local = SyncRows {
            projects: vec![],
            entries: vec![],
            sources: vec![ExternalSourceRow {
                source: ExternalSource {
                    id: "src-a".into(),
                    kind: "caldav".into(),
                    locator: "https://cal.test/shared/".into(),
                    project_id: None,
                    label: "Local Label".into(),
                    cred_ref: Some("caldav/src-a".into()),
                    cred_username: Some("alice".into()),
                },
                created_at: 100,
                updated_at: 500,
                device_id: DEV_A.into(),
            }],
            tombstones: vec![Tombstone {
                id: "entry-res".into(),
                kind: "entry".into(),
                deleted_at: 200,
                device_id: DEV_A.into(),
            }],
        };

        let mut env = sample_envelope();
        env.device_id = DEV_B.into();
        env.published_at = 600;
        env.merged_through = 100;
        // Same timestamp as DEV_A source, higher device_id DEV_B ("fed..." > "012...")
        env.sources.push(SyncedSource {
            id: "src-b".into(),
            kind: "caldav".into(),
            locator: "https://cal.test/shared/".into(),
            project_id: None,
            label: "Peer Label".into(),
            created_at: 100,
            updated_at: 500,
            device_id: DEV_B.into(),
        });
        // Resurrect `entry-res` strictly newer than tombstone @200
        env.entries.push(SyncedEntry {
            id: "entry-res".into(),
            project_id: env.projects[0].id.clone(),
            description: "Resurrected".into(),
            started_at: 1000,
            ended_at: 2000,
            source: EntrySource::Manual,
            note: None,
            external_source_id: None,
            external_uid: None,
            external_started_at: None,
            created_at: 100,
            updated_at: 250,
            device_id: DEV_B.into(),
        });

        let out = merge_snapshots(
            DEV_A,
            &local,
            &SyncConfig::default(),
            &[],
            &[env],
            &[],
            1_000,
            false,
        );
        // Tombstone deleted on resurrection
        assert!(
            out.write
                .delete_tombstones
                .contains(&("entry-res".into(), "entry".into()))
        );
        assert!(
            out.write
                .upsert_entries
                .iter()
                .any(|e| e.entry.id == "entry-res")
        );
        // Duplicate `(kind, locator)` picks higher `(500, DEV_B)` and tombstones `src-a`
        assert!(out.write.delete_sources.contains(&"src-a".into()));
        assert!(
            out.write
                .upsert_sources
                .iter()
                .any(|s| s.source.id == "src-b")
        );
        assert!(
            out.write
                .upsert_tombstones
                .iter()
                .any(|t| t.id == "src-a" && t.kind == "source")
        );
        assert_eq!(out.write.new_conflicts.len(), 1);
        assert_eq!(out.write.new_conflicts[0].record_id, "src-a");
        assert_eq!(out.write.merged_through, Some(600));
    }

    #[test]
    fn two_stores_converge_over_shared_folder_and_sockets() {
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("converge");
        let sync_dir = dir.join("shared");
        let a = spawn_test_node(&dir, "node-a", 1_700_000_000_000);
        let b = spawn_test_node(&dir, "node-b", 1_700_000_001_000);

        // Device A sets up a project, an entry, and an authenticated CalDAV source.
        let proj = a
            .client
            .call(Request::AddProject {
                name: "Client".into(),
            })
            .unwrap()
            .project()
            .unwrap();
        let entry_a = a
            .client
            .call(Request::Add {
                project_id: proj.id.clone(),
                description: "Design".into(),
                started_at: 1_699_999_000_000,
                ended_at: 1_699_999_600_000,
            })
            .unwrap()
            .entry()
            .unwrap();
        let src = a
            .client
            .call(Request::AddSource {
                kind: "caldav".into(),
                locator: "https://alice@cal.secret.test/work/".into(),
                project_id: Some(proj.id.clone()),
                label: "Work Cal".into(),
            })
            .unwrap()
            .source()
            .unwrap();
        a.client
            .call(Request::SetSourceCredential {
                id: src.id.clone(),
                username: "alice".into(),
                secret: timetrack_proto::SecretString::new("super-secret-caldav-pw"),
            })
            .unwrap()
            .source()
            .unwrap();

        // Enable sync on A.
        let enable_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        assert!(enable_a.status.enabled);
        let code = enable_a.pairing_code.unwrap();
        let shown = a
            .client
            .call(Request::SyncShowCode)
            .unwrap()
            .sync_code()
            .unwrap();
        assert_eq!(code, shown);

        // Verify raw ciphertext on disk leaks nothing.
        let raw_a = std::fs::read(sync_dir.join(&enable_a.status.device_id)).unwrap();
        assert!(!raw_a.windows(12).any(|w| w == b"super-secret"));
        assert!(!raw_a.windows(10).any(|w| w == b"cal.secret"));

        // Wrong pairing code on B rolls back cleanly without enabling or storing key.
        let bad_code = SyncKey::generate().unwrap().to_pairing_code().to_string();
        assert!(
            b.client
                .call(Request::SyncEnable {
                    folder: sync_dir.to_string_lossy().into_owned(),
                    pairing_code: Some(bad_code),
                })
                .is_err()
        );
        let status_b_err = b
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap();
        assert!(!status_b_err.enabled);
        assert!(!status_b_err.has_key);

        // Real pairing code joins cleanly, drops B's untouched "General" seed, and keeps secrets local.
        let enable_b = b
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: Some(code),
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        assert!(enable_b.status.enabled);
        let snap_b = b
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        // A's seed "General" + "Client" = 2 projects (B did NOT add a duplicate "General").
        assert_eq!(snap_b.projects.len(), 2);
        assert_eq!(snap_b.entries.len(), 1);
        assert_eq!(snap_b.entries[0].id, entry_a.id);
        let sources_b = b
            .client
            .call(Request::ListSources)
            .unwrap()
            .sources()
            .unwrap();
        assert_eq!(sources_b.len(), 1);
        assert!(!sources_b[0].has_credential);
        assert_eq!(sources_b[0].username, None);

        // B adds an entry; A pulls and converges.
        b.clock.fetch_add(5_000, Ordering::Relaxed);
        let entry_b = b
            .client
            .call(Request::Add {
                project_id: proj.id.clone(),
                description: "Review".into(),
                started_at: 1_699_999_700_000,
                ended_at: 1_699_999_900_000,
            })
            .unwrap()
            .entry()
            .unwrap();
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        a.clock.fetch_add(6_000, Ordering::Relaxed);
        let now_a = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(now_a.applied_changes >= 1);

        let snap_a = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        let snap_b2 = b
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snap_a.entries.len(), 2);
        assert_eq!(snap_b2.entries.len(), 2);
        assert!(snap_a.entries.iter().any(|e| e.id == entry_b.id));

        // Unpair B with --delete-file deletes its envelope and keeps local data intact.
        b.client
            .call(Request::SyncUnpair { delete_file: true })
            .unwrap()
            .unit()
            .unwrap();
        assert!(!sync_dir.join(&enable_b.status.device_id).exists());
        let status_b_unpaired = b
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap();
        assert!(!status_b_unpaired.enabled);
        assert!(!status_b_unpaired.has_key);
        let snap_b_after = b
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snap_b_after.entries.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_edits_both_orders_journal_and_restore_rewins() {
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("conflicts");
        let sync_dir = dir.join("shared");
        let a = spawn_test_node(&dir, "node-a", 1_700_000_000_000);
        let b = spawn_test_node(&dir, "node-b", 1_700_000_001_000);

        let proj_id = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap()
            .projects[0]
            .id
            .clone();
        let entry = a
            .client
            .call(Request::Add {
                project_id: proj_id,
                description: "Initial".into(),
                started_at: 1_699_999_000_000,
                ended_at: 1_699_999_600_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        b.client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: en_a.pairing_code,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // Both go offline: B edits at +10s (loser), A edits at +20s (winner).
        b.clock.store(1_700_000_010_000, Ordering::Relaxed);
        b.client
            .call(Request::SetText {
                id: entry.id.clone(),
                description: "Loser edit on B".into(),
            })
            .unwrap()
            .entry()
            .unwrap();

        a.clock.store(1_700_000_020_000, Ordering::Relaxed);
        a.client
            .call(Request::SetText {
                id: entry.id.clone(),
                description: "Winner edit on A".into(),
            })
            .unwrap()
            .entry()
            .unwrap();

        // Order 1: A publishes first, B pulls and records B's losing edit locally.
        a.clock.store(1_700_000_030_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        b.clock.store(1_700_000_031_000, Ordering::Relaxed);
        let sync_b = b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(sync_b.new_conflicts, 1);
        let snap_b = b
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(
            snap_b
                .entries
                .iter()
                .find(|e| e.id == entry.id)
                .unwrap()
                .description,
            "Winner edit on A"
        );

        let conflicts_b = b
            .client
            .call(Request::SyncConflicts {
                include_restored: false,
            })
            .unwrap()
            .sync_conflicts()
            .unwrap();
        assert_eq!(conflicts_b.len(), 1);
        assert!(conflicts_b[0].summary.contains("Loser edit on B"));

        // Restore by prefix on B; re-sync propagates restored version cleanly with zero new conflicts.
        let prefix: String = conflicts_b[0].id.chars().take(8).collect();
        b.clock.store(1_700_000_040_000, Ordering::Relaxed);
        b.client
            .call(Request::SyncRestoreConflict { id: prefix })
            .unwrap()
            .unit()
            .unwrap();
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        a.clock.store(1_700_000_045_000, Ordering::Relaxed);
        let sync_a2 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(sync_a2.new_conflicts, 0);
        assert_eq!(
            a.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .iter()
                .find(|e| e.id == entry.id)
                .unwrap()
                .description,
            "Loser edit on B"
        );

        // Order 2: both publish while offline (using sync_enable folder snapshots or simulated peer publish),
        // winner pulls first -> winner still preserves loser in conflict journal.
        // Simulate B publishing its envelope offline before pulling A by having B sync against an isolated folder
        // then copying its file into `sync_dir`.
        b.clock.store(1_700_000_060_000, Ordering::Relaxed);
        b.client
            .call(Request::SetText {
                id: entry.id.clone(),
                description: "Concurrent B".into(),
            })
            .unwrap()
            .entry()
            .unwrap();
        let b_id = b
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap()
            .device_id;
        // Temporarily stash A's file, publish B, restore A's file -> mirrors both publishing offline before merging.
        let a_file = sync_dir.join(&en_a.status.device_id);
        let a_backup = dir.join("a_env.bak");
        std::fs::rename(&a_file, &a_backup).unwrap();
        b.clock.store(1_700_000_061_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        std::fs::rename(&a_backup, &a_file).unwrap();

        a.clock.store(1_700_000_070_000, Ordering::Relaxed);
        a.client
            .call(Request::SetText {
                id: entry.id.clone(),
                description: "Concurrent A".into(),
            })
            .unwrap()
            .entry()
            .unwrap();
        let sync_a3 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(
            sync_a3.new_conflicts, 1,
            "winner must capture B's published loser"
        );
        assert!(sync_dir.join(&b_id).exists());
        let conf_a = a
            .client
            .call(Request::SyncConflicts {
                include_restored: false,
            })
            .unwrap()
            .sync_conflicts()
            .unwrap();
        assert!(conf_a[0].summary.contains("Concurrent B"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_vs_resurrection_folder_wipe_broken_folder_and_key_rotation() {
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("edge-cases");
        let sync_dir = dir.join("shared");
        let a = spawn_test_node(&dir, "node-a", 1_700_000_000_000);
        let b = spawn_test_node(&dir, "node-b", 1_700_000_001_000);

        let proj_id = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap()
            .projects[0]
            .id
            .clone();
        let e1 = a
            .client
            .call(Request::Add {
                project_id: proj_id.clone(),
                description: "Stay dead".into(),
                started_at: 1_699_999_000_000,
                ended_at: 1_699_999_300_000,
            })
            .unwrap()
            .entry()
            .unwrap();
        let e2 = a
            .client
            .call(Request::Add {
                project_id: proj_id.clone(),
                description: "Will resurrect".into(),
                started_at: 1_699_999_400_000,
                ended_at: 1_699_999_700_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        let en_b = b
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: en_a.pairing_code.clone(),
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // A deletes both entries at +10s.
        a.clock.store(1_700_000_010_000, Ordering::Relaxed);
        a.client
            .call(Request::DeleteEntry { id: e1.id.clone() })
            .unwrap()
            .unit()
            .unwrap();
        a.client
            .call(Request::DeleteEntry { id: e2.id.clone() })
            .unwrap()
            .unit()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // B resurrects e2 at +20s (strictly newer than delete @+10s).
        b.clock.store(1_700_000_020_000, Ordering::Relaxed);
        b.client
            .call(Request::SetText {
                id: e2.id.clone(),
                description: "Resurrected on B".into(),
            })
            .unwrap()
            .entry()
            .unwrap();
        let b_sync = b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(b_sync.status.enabled);

        a.clock.store(1_700_000_025_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        let snap_a = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        let snap_b = b
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(!snap_a.entries.iter().any(|e| e.id == e1.id));
        assert!(!snap_b.entries.iter().any(|e| e.id == e1.id));
        assert_eq!(
            snap_a
                .entries
                .iter()
                .find(|e| e.id == e2.id)
                .unwrap()
                .description,
            "Resurrected on B"
        );
        assert_eq!(
            snap_b
                .entries
                .iter()
                .find(|e| e.id == e2.id)
                .unwrap()
                .description,
            "Resurrected on B"
        );

        // Deleting all files in the sync folder loses zero local data; re-sync reseeds.
        for item in std::fs::read_dir(&sync_dir).unwrap() {
            let p = item.unwrap().path();
            let _ = std::fs::remove_file(&p);
        }
        assert!(
            !sync_dir.join(&en_a.status.device_id).exists()
                && !sync_dir.join(&en_b.status.device_id).exists()
        );
        a.clock.store(1_700_000_030_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(sync_dir.join(&en_a.status.device_id).exists());
        assert_eq!(
            a.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .len(),
            1
        );

        // Key rotation emits new code, clears stale peers, and old key cannot decrypt new file.
        b.clock.store(1_700_000_031_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        a.clock.store(1_700_000_035_000, Ordering::Relaxed);
        let new_code = a
            .client
            .call(Request::SyncRotateKey)
            .unwrap()
            .sync_code()
            .unwrap();
        assert_ne!(Some(new_code.clone()), en_a.pairing_code);
        let st_a = a
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap();
        assert!(st_a.peers.is_empty());
        // Old key on B produces a warning and keeps B's existing data untouched.
        let b_after_rot = b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(!b_after_rot.warnings.is_empty());
        assert!(b_after_rot.status.last_error.is_some());

        // Broken sync folder (replaced with a file): normal offline tracking continues untouched.
        let _ = std::fs::remove_dir_all(&sync_dir);
        std::fs::write(&sync_dir, "not a directory").unwrap();
        let sync_err = a.client.call(Request::SyncNow);
        assert!(sync_err.is_err());
        let st_broken = a
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap();
        assert!(st_broken.last_error.is_some());
        let added_offline = a
            .client
            .call(Request::QuickAdd {
                project_id: proj_id,
                duration_ms: 15 * 60_000,
            })
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!(
            a.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .iter()
                .filter(|e| e.id == added_offline.id)
                .count(),
            1
        );

        // Check FakeCredentialStore isn't flagged as dead helper even if creds isn't read here
        let _ = (a.svc, b.svc, a.creds, b.creds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmed_external_entry_dedups_across_devices_and_dismissals_stay_local() {
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("ext-dedup");
        let sync_dir = dir.join("shared");
        let ics_path = dir.join("team.ics");
        // Nov 15 & Nov 16 2023 sit within [1_700_000_000_000 - 31d, +93d].
        let ics = "\
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//TimeTrack//Test//EN
BEGIN:VEVENT
UID:standup@team
DTSTART:20231115T090000Z
DTEND:20231115T093000Z
SUMMARY:Standup
RRULE:FREQ=DAILY;COUNT=2
END:VEVENT
BEGIN:VEVENT
UID:optional-coffee@team
DTSTART:20231115T150000Z
DTEND:20231115T153000Z
SUMMARY:Coffee Chat
END:VEVENT
END:VCALENDAR
";
        std::fs::write(&ics_path, ics).unwrap();

        let a = spawn_test_node(&dir, "node-a", 1_700_000_000_000);
        let b = spawn_test_node(&dir, "node-b", 1_700_000_001_000);

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        b.client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: en_a.pairing_code,
            })
            .unwrap()
            .sync_enable()
            .unwrap();

        let src = a
            .client
            .call(Request::AddSource {
                kind: "ics_file".into(),
                locator: ics_path.to_string_lossy().into_owned(),
                project_id: None,
                label: "Team".into(),
            })
            .unwrap()
            .source()
            .unwrap();
        let ref_a = a
            .client
            .call(Request::RefreshSource {
                id: Some(src.id.clone()),
            })
            .unwrap()
            .refresh()
            .unwrap();
        assert_eq!(ref_a.inserted, 3, "2 standup occurrences + 1 coffee chat");

        let mut suggs_a = a
            .client
            .call(Request::ListSuggestions)
            .unwrap()
            .suggestions()
            .unwrap();
        suggs_a.sort_by_key(|s| (s.started_at, s.external_uid.clone()));
        // Confirm Standup occ 1; dismiss Coffee Chat; leave Standup occ 2 pending.
        let occ1 = suggs_a
            .iter()
            .find(|s| s.external_uid == "standup@team")
            .unwrap()
            .clone();
        let occ2 = suggs_a
            .iter()
            .filter(|s| s.external_uid == "standup@team")
            .nth(1)
            .unwrap()
            .clone();
        let coffee = suggs_a
            .iter()
            .find(|s| s.external_uid == "optional-coffee@team")
            .unwrap()
            .clone();

        a.clock.fetch_add(1_000, Ordering::Relaxed);
        a.client
            .call(Request::ConfirmSuggestion {
                id: occ1.id.clone(),
                project_id: None,
            })
            .unwrap()
            .entry()
            .unwrap();
        a.client
            .call(Request::DismissSuggestion {
                id: coffee.id.clone(),
            })
            .unwrap()
            .unit()
            .unwrap();

        // Sync A -> B: B receives the source and confirmed occ 1 entry.
        a.clock.fetch_add(1_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        b.clock.fetch_add(2_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // When B refreshes the same ICS file, confirmed occ 1 is NEVER suggested again,
        // while recurring occ 2 AND Coffee Chat (per-device dismissal by design) ARE suggested.
        let ref_b = b
            .client
            .call(Request::RefreshSource {
                id: Some(src.id.clone()),
            })
            .unwrap()
            .refresh()
            .unwrap();
        assert_eq!(ref_b.inserted, 2);
        let suggs_b = b
            .client
            .call(Request::ListSuggestions)
            .unwrap()
            .suggestions()
            .unwrap();
        assert!(
            !suggs_b
                .iter()
                .any(|s| s.started_at == occ1.started_at && s.external_uid == "standup@team")
        );
        assert!(
            suggs_b
                .iter()
                .any(|s| s.started_at == occ2.started_at && s.external_uid == "standup@team")
        );
        assert!(
            suggs_b
                .iter()
                .any(|s| s.external_uid == "optional-coffee@team")
        );

        // Second check: B already has occ 2 pending locally when A confirms occ 2 and syncs.
        // Pulling A's synced confirmed entry auto-confirms B's pending suggestion.
        a.clock.fetch_add(5_000, Ordering::Relaxed);
        a.client
            .call(Request::ConfirmSuggestion {
                id: occ2.id.clone(),
                project_id: None,
            })
            .unwrap()
            .entry()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        b.clock.fetch_add(6_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        let suggs_b_after = b
            .client
            .call(Request::ListSuggestions)
            .unwrap()
            .suggestions()
            .unwrap();
        assert_eq!(suggs_b_after.len(), 1);
        assert_eq!(suggs_b_after[0].external_uid, "optional-coffee@team");

        let _ = (a.svc, b.svc, a.creds, b.creds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_never_runs_ahead_of_slowest_in_horizon_peer() {
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("compact");
        let sync_dir = dir.join("shared");
        let t0 = 1_700_000_000_000i64;
        let a = spawn_test_node(&dir, "node-a", t0);
        let b = spawn_test_node(&dir, "node-b", t0 + 1_000);
        let c = spawn_test_node(&dir, "node-c", t0 + 2_000);

        let proj = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap()
            .projects[0]
            .id
            .clone();
        let entry = a
            .client
            .call(Request::Add {
                project_id: proj,
                description: "To compact".into(),
                started_at: t0 - 60_000,
                ended_at: t0 - 30_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        for node in [&b, &c] {
            node.client
                .call(Request::SyncEnable {
                    folder: sync_dir.to_string_lossy().into_owned(),
                    pairing_code: en_a.pairing_code.clone(),
                })
                .unwrap()
                .sync_enable()
                .unwrap();
        }
        a.clock.store(t0 + 5_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // A deletes the entry at t0 + 10s and publishes.
        a.clock.store(t0 + 10_000, Ordering::Relaxed);
        a.client
            .call(Request::DeleteEntry {
                id: entry.id.clone(),
            })
            .unwrap()
            .unit()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // B syncs at t0 + 20s and receives the delete (`B.merged_through = t0 + 10s`).
        // C lags (staying at t0 + 2s < delete @t0 + 10s, well within 90 days).
        b.clock.store(t0 + 20_000, Ordering::Relaxed);
        let b1 = b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(b1.compacted_tombstones, 0);

        // A syncs at t0 + 30s: C is in-horizon and lagging (`C.merged_through < delete`),
        // so A MUST NOT compact the tombstone.
        a.clock.store(t0 + 30_000, Ordering::Relaxed);
        let a2 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(a2.compacted_tombstones, 0);
        assert_eq!(a2.status.compacted_through, 0);

        // Now C syncs at t0 + 40s (pulls delete and advances C.merged_through = t0 + 20s).
        c.clock.store(t0 + 40_000, Ordering::Relaxed);
        c.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(
            c.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .is_empty()
        );

        // Next sync on A (t0 + 50s): every in-horizon peer watermark has passed delete @t0+10s -> compacts!
        a.clock.store(t0 + 50_000, Ordering::Relaxed);
        let a3 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(a3.compacted_tombstones, 1);
        assert!(a3.status.compacted_through > t0 + 10_000);

        let _ = (a.svc, b.svc, c.svc, a.creds, b.creds, c.creds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn departed_peer_over_90_days_is_quarantined_stops_blocking_and_unquarantines() {
        use super::merge::DEPARTURE_HORIZON_MS;
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("quarantine");
        let sync_dir = dir.join("shared");
        let t0 = 1_700_000_000_000i64;
        let a = spawn_test_node(&dir, "node-a", t0);
        let b = spawn_test_node(&dir, "node-b", t0 + 1_000);

        let proj = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap()
            .projects[0]
            .id
            .clone();
        let entry = a
            .client
            .call(Request::Add {
                project_id: proj,
                description: "Old deleted item".into(),
                started_at: t0 - 60_000,
                ended_at: t0 - 30_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        b.client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: en_a.pairing_code,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        a.clock.store(t0 + 5_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // A deletes at day 1; B goes dark after t0 + 1_000 and never pulls the delete.
        a.clock.store(t0 + 86_400_000, Ordering::Relaxed);
        a.client
            .call(Request::DeleteEntry {
                id: entry.id.clone(),
            })
            .unwrap()
            .unit()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // Within 90 days (e.g. day 10), B blocks compaction.
        a.clock.store(t0 + 10 * 86_400_000, Ordering::Relaxed);
        let a_day10 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(a_day10.compacted_tombstones, 0);
        assert!(!a_day10.status.peers[0].quarantined);

        // Past 90 days (day 91), A quarantines B and stops letting B block compaction!
        let day91 = t0 + DEPARTURE_HORIZON_MS + 86_400_000;
        a.clock.store(day91, Ordering::Relaxed);
        let a_day91 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(a_day91.status.peers.len(), 1);
        assert!(a_day91.status.peers[0].quarantined);
        assert_eq!(a_day91.compacted_tombstones, 1);
        assert!(a_day91.status.compacted_through >= day91);

        // When B finally wakes up on day 91 and runs sync_now (or sync_repair),
        // B pulls A BEFORE publishing -> compacted delete does NOT resurrect,
        // and once B republishes fresh, A auto-unquarantines B!
        b.clock.store(day91 + 1_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(
            b.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .is_empty(),
            "departed peer's pre-delete entry must never resurrect"
        );

        a.clock.store(day91 + 2_000, Ordering::Relaxed);
        let a_rejoined = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(!a_rejoined.status.peers[0].quarantined);
        assert!(
            a.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .is_empty()
        );

        let _ = (a.svc, b.svc, a.creds, b.creds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restored_backup_and_departed_local_writes_never_resurrect_compacted_deletes() {
        use super::merge::DEPARTURE_HORIZON_MS;
        use std::sync::atomic::Ordering;
        use timetrack_proto::socket::Request;

        let dir = temp_root("restore-repair");
        let sync_dir = dir.join("shared");
        let t0 = 1_700_000_000_000i64;
        let a = spawn_test_node(&dir, "node-a", t0);
        let b = spawn_test_node(&dir, "node-b", t0 + 1_000);

        let proj = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap()
            .projects[0]
            .id
            .clone();
        let victim = a
            .client
            .call(Request::Add {
                project_id: proj.clone(),
                description: "Pre-backup deleted later".into(),
                started_at: t0 - 60_000,
                ended_at: t0 - 30_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        let en_a = a
            .client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: None,
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        b.client
            .call(Request::SyncEnable {
                folder: sync_dir.to_string_lossy().into_owned(),
                pairing_code: en_a.pairing_code.clone(),
            })
            .unwrap()
            .sync_enable()
            .unwrap();
        a.clock.store(t0 + 5_000, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        b.clock.store(t0 + 6_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();

        // Take a §17-style stopped copy of Node B's SQLite database + WAL while both have `victim`.
        let bak_dir = dir.join("backup-b");
        std::fs::create_dir_all(&bak_dir).unwrap();
        std::fs::copy(dir.join("node-b/store.db"), bak_dir.join("store.db")).unwrap();
        if dir.join("node-b/store.db-wal").exists() {
            std::fs::copy(
                dir.join("node-b/store.db-wal"),
                bak_dir.join("store.db-wal"),
            )
            .unwrap();
        }
        // Also snapshot B's stale envelope bytes from that time so we can test quarantine.
        let b_dev_id = b
            .client
            .call(Request::SyncStatus)
            .unwrap()
            .sync_status()
            .unwrap()
            .device_id;
        let stale_b_env_bytes = std::fs::read(sync_dir.join(&b_dev_id)).unwrap();

        // Now A deletes `victim` at t0 + 10s and B (before losing disk) syncs the delete, then A compacts.
        a.clock.store(t0 + 10_000, Ordering::Relaxed);
        a.client
            .call(Request::DeleteEntry {
                id: victim.id.clone(),
            })
            .unwrap()
            .unit()
            .unwrap();
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        b.clock.store(t0 + 15_000, Ordering::Relaxed);
        b.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        a.clock.store(t0 + 20_000, Ordering::Relaxed);
        let compacted = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert_eq!(compacted.compacted_tombstones, 1);
        assert!(compacted.status.compacted_through > t0 + 10_000);

        // Simulate putting the pre-compaction stale envelope from backup back in the folder:
        // A MUST quarantine it instead of resurrecting `victim`.
        std::fs::write(sync_dir.join(&b_dev_id), &stale_b_env_bytes).unwrap();
        a.clock.store(t0 + 25_000, Ordering::Relaxed);
        let a_quar = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(a_quar.status.peers.iter().any(|p| p.quarantined));
        assert!(
            a.client
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .is_empty(),
            "stale envelope behind compacted_through must be quarantined, not resurrect"
        );

        // Now restore the backup DB into a fresh node directory (`node-b-restored`) >90 days later
        // (matching an old backup restore), AND write a brand-new local-only entry BEFORE repairing!
        let restored_dir = dir.join("node-b-restored");
        std::fs::create_dir_all(&restored_dir).unwrap();
        std::fs::copy(bak_dir.join("store.db"), restored_dir.join("store.db")).unwrap();
        if bak_dir.join("store.db-wal").exists() {
            std::fs::copy(
                bak_dir.join("store.db-wal"),
                restored_dir.join("store.db-wal"),
            )
            .unwrap();
        }
        let day45 = t0 + 45 * 86_400_000;
        let day95 = t0 + DEPARTURE_HORIZON_MS + 5 * 86_400_000;
        // Keep A active across the 95-day span so A stays within the 90d horizon and publishes before B repairs.
        a.clock.store(day45, Ordering::Relaxed);
        a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        a.clock.store(day95 - 1_000, Ordering::Relaxed);
        let a_day95 = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(!a_day95.status.needs_repair);

        let creds_restored = std::sync::Arc::new(FakeCredentialStore::new());
        // Restored machine keeps the sync key in its OS store (paired with `en_a.pairing_code`).
        super::key::store_sync_key(
            &*creds_restored,
            &super::key::SyncKey::from_pairing_code(en_a.pairing_code.as_deref().unwrap()).unwrap(),
        )
        .unwrap();
        let clock_r = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(day95));
        let cr = std::sync::Arc::clone(&clock_r);
        let store_r = SqliteStore::open_at(&restored_dir.join("store.json")).unwrap();
        let svc_r = std::sync::Arc::new(
            crate::interface::EntryService::new_with_credentials_and_clock(
                store_r,
                creds_restored.clone(),
                std::sync::Arc::new(move || cr.load(Ordering::Relaxed)),
            )
            .unwrap(),
        );
        let sock_r = restored_dir.join(format!("s-r-{}.sock", std::process::id()));
        crate::socket::serve_at(std::sync::Arc::clone(&svc_r), sock_r.clone()).unwrap();
        let client_r = timetrack_proto::socket::SocketClient::new(sock_r);

        // Confirm restored backup still holds `victim` locally before repair, and add a new local entry.
        assert_eq!(
            client_r
                .call(Request::Snapshot)
                .unwrap()
                .snapshot()
                .unwrap()
                .entries
                .len(),
            1
        );
        clock_r.store(day95 + 1_000, Ordering::Relaxed);
        let kept_local = client_r
            .call(Request::Add {
                project_id: proj,
                description: "Offline post-restore work".into(),
                started_at: day95 - 10_000,
                ended_at: day95 - 5_000,
            })
            .unwrap()
            .entry()
            .unwrap();

        // Run one-action `SyncRepair`: drops stale file & compacted `victim`, preserves `kept_local`, reseeds.
        clock_r.store(day95 + 2_000, Ordering::Relaxed);
        let repaired = client_r
            .call(Request::SyncRepair)
            .unwrap()
            .sync_now()
            .unwrap();
        assert!(!repaired.status.needs_repair);

        let snap_r = client_r
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(
            !snap_r.entries.iter().any(|e| e.id == victim.id),
            "compacted delete must NEVER resurrect from restored backup"
        );
        assert!(
            snap_r.entries.iter().any(|e| e.id == kept_local.id),
            "local-only write made while departed must survive re-pair"
        );

        // Node A pulls repaired B: A un-quarantines B, receives `kept_local`, and `victim` stays dead!
        a.clock.store(day95 + 3_000, Ordering::Relaxed);
        let a_final = a.client.call(Request::SyncNow).unwrap().sync_now().unwrap();
        assert!(!a_final.status.peers.iter().any(|p| p.quarantined));
        let snap_a_final = a
            .client
            .call(Request::Snapshot)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snap_a_final.entries.len(), 1);
        assert_eq!(snap_a_final.entries[0].id, kept_local.id);

        let _ = (a.svc, b.svc, svc_r, a.creds, b.creds, creds_restored);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
