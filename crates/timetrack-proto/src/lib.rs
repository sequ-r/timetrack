/* proto/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The wire contract and a client for the TimeTrack service.
//!
//! Both front ends depend on this rather than on each other, so the GUI
//! (a flatpak) and the CLI (a static binary) agree on one definition. It
//! carries no GUI or storage code, which keeps the CLI's static build small.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Well-known bus name. Also the app-id used by the desktop entry.
pub const BUS_NAME: &str = "org.sequ.timetrack";

/// Object path of the single exported interface.
pub const OBJECT_PATH: &str = "/org/sequ/timetrack";

/// Interface name.
pub const INTERFACE: &str = "org.sequ.timetrack.Timer";

/// A snapshot of the service's state.
///
/// A plain serializable struct rather than a live D-Bus type: clients get a
/// consistent point-in-time view, and adding a field is a non-breaking change
/// for both sides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The in-progress entry, if one is running.
    pub running: Option<EntryView>,
    /// Finished and running entries, most recent first.
    pub entries: Vec<EntryView>,
    /// Total tracked milliseconds, counting the running entry to now.
    pub total_ms: i64,
}

impl Snapshot {
    /// The entry currently running, if any.
    pub fn running(&self) -> Option<&EntryView> {
        self.running.as_ref()
    }
}

/// One entry as seen by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryView {
    pub id: String,
    pub description: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl EntryView {
    pub fn is_running(&self) -> bool {
        self.ended_at.is_none()
    }

    /// Duration in milliseconds, using `now` while still running.
    pub fn duration_ms(&self, now: i64) -> i64 {
        (self.ended_at.unwrap_or(now) - self.started_at).max(0)
    }

    /// Description, or a placeholder when the user left it empty.
    pub fn label(&self) -> &str {
        if self.description.is_empty() {
            "(no description)"
        } else {
            &self.description
        }
    }
}

/// Convert a core entry into its wire representation.
pub fn to_view(e: &timetrack_core::Entry) -> EntryView {
    EntryView {
        id: e.id.clone(),
        description: e.description.clone(),
        started_at: e.started_at,
        ended_at: e.ended_at,
    }
}

/// A connection to the running service.
#[derive(Clone)]
pub struct Client {
    proxy: zbus::Proxy<'static>,
}

impl Client {
    /// Connect to the service, with a short timeout so a missing service
    /// produces a clear message rather than a hang.
    pub async fn connect() -> anyhow::Result<Self> {
        // NOTE: do *not* call `.name(BUS_NAME)` here. On a client connection
        // that asks the bus to hand us the well-known name, which only ever
        // succeeds if nothing else owns it — so it would block forever
        // against a healthy service and report success when the service is
        // absent. A client must connect anonymously and address the owner by
        // name at call time, which is what `Proxy::new` does.
        let conn = zbus::connection::Builder::session()?.build().await?;
        let proxy = zbus::Proxy::new(
            &conn,
            BUS_NAME,
            OBJECT_PATH,
            INTERFACE,
        )
        .await?;
        Ok(Client { proxy })
    }

    /// Call a no-argument method and decode its JSON reply.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
    ) -> anyhow::Result<T> {
        let reply = self.proxy.call_method(method, &()).await?;
        let wire: String = reply.body().deserialize()?;
        Ok(serde_json::from_str(&wire)?)
    }

    pub async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        self.call("Snapshot").await
    }

    pub async fn start(&self, description: &str) -> anyhow::Result<EntryView> {
        let reply = self.proxy.call_method("Start", &(description,)).await?;
        let wire: String = reply.body().deserialize()?;
        Ok(serde_json::from_str(&wire)?)
    }

    pub async fn stop(&self) -> anyhow::Result<EntryView> {
        self.call("Stop").await
    }

    pub async fn cancel(&self) -> anyhow::Result<EntryView> {
        self.call("Cancel").await
    }

    pub async fn remove(&self, id: &str) -> anyhow::Result<()> {
        self.proxy.call_method("Remove", &(id,)).await?;
        Ok(())
    }

    pub async fn rename(&self, id: &str, description: &str) -> anyhow::Result<EntryView> {
        let reply = self
            .proxy
            .call_method("Rename", &(id, description))
            .await?;
        let wire: String = reply.body().deserialize()?;
        Ok(serde_json::from_str(&wire)?)
    }

    /// Poll until the service appears on the bus, or give up.
    pub async fn wait_for_service(timeout: Duration) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(c) = Client::connect().await {
                return Ok(c);
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "the timetrack service ({BUS_NAME}) did not appear on the bus.\n\
                     Start it with:  timetrack-service\n\
                     It is normally started on demand by the systemd user unit."
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        // Clients deserialise exactly this shape, so a struct change that
        // breaks the round trip would break them at runtime.
        let snap = Snapshot {
            running: Some(EntryView {
                id: "e1".into(),
                description: "work".into(),
                started_at: 1,
                ended_at: None,
            }),
            entries: vec![EntryView {
                id: "e1".into(),
                description: "work".into(),
                started_at: 1,
                ended_at: None,
            }],
            total_ms: 42,
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snap);
    }

    #[test]
    fn default_snapshot_is_empty_but_valid() {
        let text = serde_json::to_string(&Snapshot::default()).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert!(back.entries.is_empty());
        assert!(back.running.is_none());
    }

    #[test]
    fn bus_name_matches_object_path_convention() {
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
    }

    #[test]
    fn duration_counts_to_now_while_running() {
        let e = EntryView {
            id: "e1".into(),
            description: String::new(),
            started_at: 1_000,
            ended_at: None,
        };
        assert_eq!(e.duration_ms(4_500), 3_500);
        assert!(e.is_running());
    }

    #[test]
    fn duration_uses_end_time_when_finished() {
        let e = EntryView {
            id: "e1".into(),
            description: String::new(),
            started_at: 1_000,
            ended_at: Some(2_000),
        };
        // `now` is irrelevant once the entry has ended.
        assert_eq!(e.duration_ms(999_999), 1_000);
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        let e = EntryView {
            id: "e1".into(),
            description: String::new(),
            started_at: 5_000,
            ended_at: None,
        };
        assert_eq!(e.duration_ms(1_000), 0);
    }

    #[test]
    fn empty_description_gets_a_placeholder() {
        let e = EntryView {
            id: "e1".into(),
            description: String::new(),
            started_at: 0,
            ended_at: Some(1),
        };
        assert_eq!(e.label(), "(no description)");
    }
}
