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
//! carries no GUI and no storage code, which keeps the CLI's static build
//! small.
//!
//! # Why payloads are JSON strings
//!
//! Every method takes and returns a single `s` holding JSON. A hand-written
//! D-Bus signature per call would mean two definitions of every type -- one
//! in the interface, one in the client -- and they would drift. With JSON the
//! serde structs below are the single definition, so a field added on the
//! service side arrives on the client side with no signature change at all.
//!
//! # The running timer is gone
//!
//! There is deliberately no `Running` field, `Start` or `Stop` method: every
//! entry is closed (REQUIREMENTS §7). The old stopwatch surface was removed
//! rather than deprecated, so nothing here can resurrect it.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Well-known bus name. Also the app-id used by the desktop entry.
pub const BUS_NAME: &str = "org.sequ.timetrack";

/// Object path of the single exported interface.
pub const OBJECT_PATH: &str = "/org/sequ/timetrack";

/// Interface name.
pub const INTERFACE: &str = "org.sequ.timetrack.Entries";

/// How an entry was recorded, mirroring `timetrack_core::EntrySource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntrySource {
    Manual,
    QuickAdd,
}

/// One entry as seen by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryView {
    pub id: String,
    pub project_id: String,
    pub description: String,
    pub started_at: i64,
    /// Always present: entries are closed on creation (REQUIREMENTS §7).
    pub ended_at: i64,
    pub source: EntrySource,
    #[serde(default)]
    pub note: Option<String>,
}

impl EntryView {
    /// How long this entry ran. No `now` parameter, because nothing is
    /// running.
    pub fn duration_ms(&self) -> i64 {
        (self.ended_at - self.started_at).max(0)
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

/// One project as seen by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub colour: Option<u32>,
    #[serde(default)]
    pub archived: bool,
}

/// Totals for one period, mirroring `timetrack_core::aggregate::Totals`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotalsView {
    pub total_ms: i64,
    pub per_project: std::collections::BTreeMap<String, i64>,
    pub entry_count: usize,
}

/// A consistent snapshot of the service's state.
///
/// One call, one point in time: a client renders this without the risk of
/// mixing a stale project list with fresh entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// All projects, including archived ones -- archived projects still
    /// contribute to historical totals (REQUIREMENTS §14), so a client
    /// filtering them out for display would lose the ability to show them.
    pub projects: Vec<ProjectView>,
    /// Entries, most recent first.
    pub entries: Vec<EntryView>,
    /// Totals for the ISO week containing `now`.
    pub week: TotalsView,
    /// Totals for the calendar month containing `now`.
    pub month: TotalsView,
    /// Total across all time.
    pub total_ms: i64,
    /// Local days whose summed time exceeds 24h, for the 24h warning
    /// (REQUIREMENTS §4). Values are local day indices.
    #[serde(default)]
    pub over_24h_days: Vec<i64>,
    /// The zone's UTC offset in milliseconds, so a client can bucket days
    /// the same way the service did.
    #[serde(default)]
    pub local_offset_ms: i64,
}

/// Convert a core entry into its wire representation.
pub fn entry_to_view(e: &timetrack_core::Entry) -> EntryView {
    EntryView {
        id: e.id.clone(),
        project_id: e.project_id.clone(),
        description: e.description.clone(),
        started_at: e.started_at,
        ended_at: e.ended_at,
        source: match e.source {
            timetrack_core::EntrySource::Manual => EntrySource::Manual,
            timetrack_core::EntrySource::QuickAdd => EntrySource::QuickAdd,
        },
        note: e.note.clone(),
    }
}

/// Convert a core project into its wire representation.
pub fn project_to_view(p: &timetrack_core::Project) -> ProjectView {
    ProjectView {
        id: p.id.clone(),
        name: p.name.clone(),
        colour: p.colour,
        archived: p.archived,
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
        // succeeds if nothing else owns it -- so it would block forever
        // against a healthy service and report success when the service is
        // absent. A client must connect anonymously and address the owner by
        // name at call time, which is what `Proxy::new` does.
        let conn = zbus::connection::Builder::session()?.build().await?;
        let proxy = zbus::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
        Ok(Client { proxy })
    }

    /// Call a no-argument method and decode its JSON reply.
    async fn call<T: serde::de::DeserializeOwned>(&self, method: &str) -> anyhow::Result<T> {
        let reply = self.proxy.call_method(method, &()).await?;
        let wire: String = reply.body().deserialize()?;
        Ok(serde_json::from_str(&wire)?)
    }

    /// Call a one-argument method and decode its JSON reply.
    ///
    /// `A` needs zbus's own `DynamicType`, not just `Serialize`: zbus derives
    /// the D-Bus signature from the type, so a plain `Serialize` bound is not
    /// enough to call a method.
    async fn call1<
        T: serde::de::DeserializeOwned,
        A: serde::Serialize + zbus::zvariant::DynamicType,
    >(
        &self,
        method: &str,
        arg: &A,
    ) -> anyhow::Result<T> {
        let reply = self.proxy.call_method(method, arg).await?;
        let wire: String = reply.body().deserialize()?;
        Ok(serde_json::from_str(&wire)?)
    }

    pub async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        self.call("Snapshot").await
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    /// Method 1: an explicit start and end.
    pub async fn add(
        &self,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> anyhow::Result<EntryView> {
        self.call1("Add", &(project_id, description, started_at, ended_at))
            .await
    }

    /// Method 2: a duration ending now.
    pub async fn add_duration(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> anyhow::Result<EntryView> {
        self.call1("AddDuration", &(project_id, description, duration_ms))
            .await
    }

    /// Method 3: a duration ending at a given instant.
    pub async fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> anyhow::Result<EntryView> {
        self.call1(
            "AddDurationEnding",
            &(project_id, description, duration_ms, ended_at),
        )
        .await
    }

    /// Method 4: one of the quick-add buckets, ending now.
    pub async fn quick_add(&self, project_id: &str, duration_ms: i64) -> anyhow::Result<EntryView> {
        self.call1("QuickAdd", &(project_id, duration_ms)).await
    }

    // --- editing ---

    /// Move an entry's interval. This is method 1's "remove" side: it
    /// shortens by moving an endpoint and never deletes (REQUIREMENTS §5).
    pub async fn set_times(
        &self,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> anyhow::Result<EntryView> {
        self.call1("SetTimes", &(id, started_at, ended_at)).await
    }

    /// Undo exactly the entry a quick-add created.
    pub async fn undo_quick_add(&self, id: &str) -> anyhow::Result<()> {
        self.proxy.call_method("UndoQuickAdd", &(id,)).await?;
        Ok(())
    }

    /// Explicit deletion. The only way an entry is removed on purpose.
    pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        self.proxy.call_method("DeleteEntry", &(id,)).await?;
        Ok(())
    }

    pub async fn split(&self, id: &str, at_ms: i64) -> anyhow::Result<(EntryView, EntryView)> {
        self.call1("Split", &(id, at_ms)).await
    }

    /// Merge entries into one spanning the union of their intervals.
    ///
    /// The result's duration is the union, so merging *overlapping* entries
    /// reduces the total by the overlap. Clients must say so before calling.
    pub async fn merge(&self, ids: &[String]) -> anyhow::Result<EntryView> {
        self.call1("Merge", &ids.to_vec()).await
    }

    // --- projects ---

    pub async fn add_project(&self, name: &str) -> anyhow::Result<ProjectView> {
        self.call1("AddProject", &name).await
    }

    pub async fn set_archived(&self, id: &str, archived: bool) -> anyhow::Result<ProjectView> {
        self.call1("SetArchived", &(id, archived)).await
    }

    pub async fn delete_project(&self, id: &str) -> anyhow::Result<()> {
        self.proxy.call_method("DeleteProject", &(id,)).await?;
        Ok(())
    }

    /// CSV export (REQUIREMENTS §10). `scope` is `week`, `month` or `all`;
    /// returns the CSV text including the header row.
    pub async fn export_csv(&self, scope: &str) -> anyhow::Result<String> {
        self.call1("ExportCsv", &scope).await
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

    fn entry() -> EntryView {
        EntryView {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "work".into(),
            started_at: 1_000,
            ended_at: 4_500,
            source: EntrySource::Manual,
            note: None,
        }
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        // Clients deserialise exactly this shape, so a struct change that
        // breaks the round trip would break them at runtime.
        let mut snap = Snapshot {
            entries: vec![entry()],
            local_offset_ms: 3_600_000,
            ..Default::default()
        };
        snap.projects.push(ProjectView {
            id: "p1".into(),
            name: "Work".into(),
            colour: Some(0x2ea043),
            archived: false,
        });
        snap.week.total_ms = 3_500;
        let text = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snap);
    }

    #[test]
    fn default_snapshot_is_empty_but_valid() {
        let text = serde_json::to_string(&Snapshot::default()).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert!(back.entries.is_empty());
        assert!(back.projects.is_empty());
        assert_eq!(back.total_ms, 0);
    }

    #[test]
    fn bus_name_matches_object_path_convention() {
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
    }

    #[test]
    fn duration_needs_no_clock() {
        assert_eq!(entry().duration_ms(), 3_500);
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        let mut e = entry();
        e.started_at = 5_000;
        e.ended_at = 1_000;
        assert_eq!(e.duration_ms(), 0);
    }

    #[test]
    fn empty_description_gets_a_placeholder() {
        let mut e = entry();
        e.description = String::new();
        assert_eq!(e.label(), "(no description)");
    }

    #[test]
    fn there_is_no_running_entry_on_the_wire() {
        // A regression guard: the stopwatch's `running` field must not creep
        // back into the contract, because it would imply a state that no
        // longer exists in the model.
        let text = serde_json::to_string(&Snapshot::default()).unwrap();
        assert!(
            !text.contains("running"),
            "unexpected running field: {text}"
        );
    }

    #[test]
    fn entry_conversion_preserves_the_source() {
        let core = timetrack_core::Entry {
            id: "e9".into(),
            project_id: "p2".into(),
            description: "note".into(),
            started_at: 5,
            ended_at: 10,
            source: timetrack_core::EntrySource::QuickAdd,
            note: Some("n".into()),
        };
        let view = entry_to_view(&core);
        assert_eq!(view.source, EntrySource::QuickAdd);
        assert_eq!(view.duration_ms(), 5);
        assert_eq!(view.note.as_deref(), Some("n"));
    }

    #[test]
    fn totals_view_round_trips() {
        let mut t = TotalsView::default();
        t.total_ms = 100;
        t.per_project.insert("p1".into(), 100);
        t.entry_count = 1;
        let text = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<TotalsView>(&text).unwrap(), t);
    }
}
