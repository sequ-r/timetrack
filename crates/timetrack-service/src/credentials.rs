/* service/credentials.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Per-source secrets in the OS credential store (REQUIREMENTS §19, #149).
//!
//! Each CalDAV source's password or token lives in the native store —
//! Secret Service on Linux, Credential Manager on Windows, Keychain on
//! macOS, all through the `keyring` crate — keyed by
//! [`credential_ref`]. SQLite holds only that reference plus the
//! non-secret login name (schema v4); the secret itself never reaches
//! the database, a log line, a process argument, or an export.
//!
//! # Shape
//!
//! [`CredentialStore`] is the injectable seam: production serves
//! [`OsCredentialStore`], tests inject the fake. The keyring entry is
//! addressed by the app id as the service and the credential reference
//! as the account, so every TimeTrack secret is recognizable in a
//! key manager yet namespaced away from other apps.
//!
//! # Lifecycle
//!
//! - Setting overwrites: a second `set` for one source replaces the
//!   stored secret, and the database reference is re-recorded.
//! - Removing deletes the OS entry first and clears the reference only
//!   after, so a failed delete leaves the source exactly as it was
//!   instead of silently downgrading it to the env-var fallback below.
//! - Deleting a source purges its OS entry best-effort then removes the
//!   row, which is the only reference the app holds: when the store
//!   answers, nothing is left behind; when it does not (locked,
//!   unreachable), an orphaned entry may remain in the OS store, but it
//!   is unreachable through the app and can be deleted in any key
//!   manager. Deletion is never held hostage by a broken store.
//!
//! # When the store is unavailable
//!
//! Every operation fails with [`CredentialError::Unavailable`] naming
//! the cause (no Secret Service in the session, a locked collection)
//! instead of failing silently or falling back to an insecure store.
//! There is no file fallback: an encrypted-file fallback would need
//! its own key management, which is the problem the OS store solves.
//!
//! # The environment fallback
//!
//! A source with *no* credential reference still reads
//! `TIMETRACK_CALDAV_PASSWORD`, exactly as before this module existed:
//! headless setups and the very first fetch keep working without a
//! keyring. The moment a reference exists the fallback stops applying
//! to that source — a stored credential that goes missing is an error
//! telling the user to set it again, never a silent downgrade to a
//! weaker secret.
//!
//! # Platforms
//!
//! All three backends come from the one `keyring` dependency (Linux via
//! Secret Service over D-Bus, which needs a running keyring such as
//! GNOME Keyring or KWallet; Windows and macOS via their native APIs).
//! The crate is built with default features off (`v1` only), so the
//! service gains no tokio runtime and no OpenSSL — pure-Rust crypto,
//! the async-io executor the workspace already uses. Behaviour is
//! unit-tested on Linux against the fake; the Windows and macOS
//! backends ride the same `keyring` calls but have not been
//! runtime-verified in this tree (no such runner here), so a failure
//! there surfaces as `Unavailable` with the platform's own wording.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

/// The keyring service every TimeTrack secret is stored under: the app
/// id, so the entries are recognizable in any key manager.
pub const KEYRING_SERVICE: &str = "org.sequ.timetrack";

/// The credential reference for a source: stable (derived from the
/// source id, which never changes) and namespaced by kind, so a future
/// second secret kind cannot collide with CalDAV's.
pub fn credential_ref(source_id: &str) -> String {
    format!("caldav/{source_id}")
}

/// Why a credential operation failed. Carries no secret: messages name
/// the reference and the cause, never the stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// No usable credential store: none in the session, locked, or
    /// otherwise inaccessible. The message says which, actionably.
    Unavailable(String),
    /// Anything else the store reported, in its own wording.
    Other(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Unavailable(why) => write!(f, "{why}"),
            CredentialError::Other(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Per-source secrets, injectable so tests never touch a real store.
pub trait CredentialStore: Send + Sync {
    /// The secret stored under `reference`, or `None` when no entry
    /// exists. A missing entry is not an error — callers turn it into
    /// their own "set it again" guidance.
    fn get(&self, reference: &str) -> Result<Option<String>, CredentialError>;
    /// Store (or overwrite) the secret under `reference`.
    fn set(&self, reference: &str, secret: &str) -> Result<(), CredentialError>;
    /// Delete the entry under `reference`. Deleting what is not there
    /// succeeds, so remove flows stay idempotent.
    fn delete(&self, reference: &str) -> Result<(), CredentialError>;
}

/// The production backend: the OS credential store via `keyring`.
#[derive(Debug, Default)]
pub struct OsCredentialStore;

impl OsCredentialStore {
    pub fn new() -> Self {
        OsCredentialStore
    }
}

fn no_store() -> CredentialError {
    CredentialError::Unavailable(
        "no OS credential store is available in this session (on Linux, a Secret Service \
         keyring such as GNOME Keyring or KWallet must be running and unlocked)"
            .into(),
    )
}

fn map_keyring(op: &str, error: keyring::Error) -> CredentialError {
    match error {
        keyring::Error::NoDefaultStore => no_store(),
        keyring::Error::NoStorageAccess(platform) => CredentialError::Unavailable(format!(
            "the OS credential store refused {op}: {platform} \
             (it may be locked; unlock it and retry)"
        )),
        other => CredentialError::Other(format!("the OS credential store failed to {op}: {other}")),
    }
}

impl CredentialStore for OsCredentialStore {
    fn get(&self, reference: &str) -> Result<Option<String>, CredentialError> {
        let entry =
            keyring::Entry::new(KEYRING_SERVICE, reference).map_err(|e| map_keyring("read", e))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring("read", e)),
        }
    }

    fn set(&self, reference: &str, secret: &str) -> Result<(), CredentialError> {
        let entry =
            keyring::Entry::new(KEYRING_SERVICE, reference).map_err(|e| map_keyring("write", e))?;
        entry
            .set_password(secret)
            .map_err(|e| map_keyring("write", e))
    }

    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, reference)
            .map_err(|e| map_keyring("delete", e))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring("delete", e)),
        }
    }
}

/// Resolve a CalDAV source's Basic-auth pair: the stored credential
/// when the source has a reference, the documented environment
/// fallback when it has none (`env_password` is passed in rather than
/// read here, so the matrix is unit-testable without touching the
/// process environment).
///
/// Returns `None` for an anonymous source (no username anywhere, no
/// password anywhere). A username with no password authenticates with
/// an empty password, as the pre-credential fetch did.
pub fn resolve_caldav_auth(
    cred_ref: Option<&str>,
    cred_username: Option<&str>,
    locator: &str,
    lookup: &dyn Fn(&str) -> Result<Option<String>, CredentialError>,
    env_password: Option<&str>,
) -> Result<Option<(String, String)>, String> {
    let (_, locator_user) = crate::caldav::split_userinfo(locator)?;
    let username = cred_username.map(str::to_string).or(locator_user);
    match cred_ref {
        Some(reference) => {
            let secret = lookup(reference)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    "the OS credential store holds no entry for this source; set it again \
                     with: timetrack source-set-credential <id>"
                        .to_string()
                })?;
            let username = username.ok_or_else(|| {
                "this source has a credential but no username; set it again \
                 with: timetrack source-set-credential <id> --username <name>"
                    .to_string()
            })?;
            Ok(Some((username, secret)))
        }
        None => Ok(match (username, env_password) {
            (Some(user), Some(password)) => Some((user, password.to_string())),
            (Some(user), None) => Some((user, String::new())),
            _ => None,
        }),
    }
}

/// The test backend: an in-memory map, with an optional failure switch
/// so unavailable-store paths are testable without a real daemon.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeCredentialStore {
    entries: Mutex<HashMap<String, String>>,
    down: Mutex<Option<String>>,
}

#[cfg(test)]
impl FakeCredentialStore {
    pub fn new() -> Self {
        FakeCredentialStore::default()
    }

    /// Fail every later operation as unavailable, until [`Self::heal`].
    pub fn fail_with(&self, message: &str) {
        *self.down.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.to_string());
    }

    pub fn heal(&self) {
        *self.down.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn check(&self) -> Result<(), CredentialError> {
        if let Some(message) = self.down.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Err(CredentialError::Unavailable(message));
        }
        Ok(())
    }
}

#[cfg(test)]
impl CredentialStore for FakeCredentialStore {
    fn get(&self, reference: &str) -> Result<Option<String>, CredentialError> {
        self.check()?;
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(reference)
            .cloned())
    }

    fn set(&self, reference: &str, secret: &str) -> Result<(), CredentialError> {
        self.check()?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(reference.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        self.check()?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(
        entries: &HashMap<String, String>,
    ) -> impl Fn(&str) -> Result<Option<String>, CredentialError> + '_ {
        |reference| Ok(entries.get(reference).cloned())
    }

    #[test]
    fn refs_are_namespaced_and_stable() {
        assert_eq!(credential_ref("src-abc"), "caldav/src-abc");
        assert_eq!(KEYRING_SERVICE, "org.sequ.timetrack");
    }

    #[test]
    fn a_stored_credential_wins_over_everything_else() {
        let entries = HashMap::from([("caldav/src-1".to_string(), "stored".to_string())]);
        // The env var is ignored once a reference exists — no silent
        // downgrade to a weaker secret.
        assert_eq!(
            resolve_caldav_auth(
                Some("caldav/src-1"),
                Some("alice"),
                "https://user@cal.test/u/",
                &lookup(&entries),
                Some("env"),
            )
            .unwrap(),
            Some(("alice".to_string(), "stored".to_string()))
        );
        // The stored username wins over the locator's userinfo.
        assert_eq!(
            resolve_caldav_auth(
                Some("caldav/src-1"),
                Some("alice"),
                "https://user@cal.test/u/",
                &lookup(&entries),
                None,
            )
            .unwrap()
            .unwrap()
            .0,
            "alice"
        );
    }

    #[test]
    fn a_missing_entry_is_an_actionable_error_not_a_fallback() {
        let entries = HashMap::new();
        let err = resolve_caldav_auth(
            Some("caldav/src-1"),
            Some("alice"),
            "https://cal.test/u/",
            &lookup(&entries),
            Some("env"),
        )
        .unwrap_err();
        assert!(
            err.contains("source-set-credential"),
            "the error says how to fix it: {err}"
        );
        assert!(
            !err.contains("env"),
            "and never mentions the fallback: {err}"
        );
        // A store failure surfaces as-is, still with no fallback.
        let down = |_: &str| -> Result<Option<String>, CredentialError> {
            Err(CredentialError::Unavailable("locked".into()))
        };
        assert_eq!(
            resolve_caldav_auth(
                Some("caldav/src-1"),
                Some("alice"),
                "https://cal.test/u/",
                &down,
                Some("env"),
            )
            .unwrap_err(),
            "locked"
        );
    }

    #[test]
    fn without_a_reference_the_env_var_is_the_documented_fallback() {
        let entries = HashMap::new();
        assert_eq!(
            resolve_caldav_auth(
                None,
                None,
                "https://alice@cal.test/u/",
                &lookup(&entries),
                Some("env"),
            )
            .unwrap(),
            Some(("alice".to_string(), "env".to_string()))
        );
        // A username alone authenticates with an empty password, as the
        // pre-credential fetch did.
        assert_eq!(
            resolve_caldav_auth(
                None,
                None,
                "https://alice@cal.test/u/",
                &lookup(&entries),
                None,
            )
            .unwrap(),
            Some(("alice".to_string(), String::new()))
        );
        // Nothing anywhere is an anonymous fetch.
        assert_eq!(
            resolve_caldav_auth(None, None, "https://cal.test/u/", &lookup(&entries), None,)
                .unwrap(),
            None
        );
        // A locator password is still refused, fallback or not.
        assert!(
            resolve_caldav_auth(
                None,
                None,
                "https://alice:secret@cal.test/u/",
                &lookup(&entries),
                Some("env"),
            )
            .is_err()
        );
    }

    #[test]
    fn the_fake_round_trips_and_fails_on_command() {
        let fake = FakeCredentialStore::new();
        assert_eq!(fake.get("r").unwrap(), None);
        fake.set("r", "s3cr3t").unwrap();
        assert_eq!(fake.get("r").unwrap().as_deref(), Some("s3cr3t"));
        fake.delete("r").unwrap();
        assert_eq!(fake.get("r").unwrap(), None);
        // Deleting what is not there succeeds.
        fake.delete("r").unwrap();

        fake.fail_with("daemon away");
        assert!(matches!(
            fake.get("r"),
            Err(CredentialError::Unavailable(_))
        ));
        assert!(fake.set("r", "x").is_err());
        assert!(fake.delete("r").is_err());
        fake.heal();
        fake.set("r", "back").unwrap();
        assert_eq!(fake.get("r").unwrap().as_deref(), Some("back"));
    }

    #[test]
    fn credential_errors_carry_no_secret() {
        // The error type only ever formats its message, which callers
        // build from references and causes — there is no field a
        // secret could hide in.
        let err = CredentialError::Unavailable("locked".into());
        assert_eq!(err.to_string(), "locked");
        assert_eq!(format!("{err:?}"), "Unavailable(\"locked\")");
    }
}
