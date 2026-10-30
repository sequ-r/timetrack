/* service/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack service.
//!
//! Owns the store and serves it on the session bus. Both front ends are
//! clients: the GPUI GUI and the ratatui CLI, which means tracked data
//! outlives either one.
//!
//! # Two interfaces, one core
//!
//! The same object serves the original JSON-in-string interface and
//! the typed `Entries2` (#110, ADR 0012), both over the one
//! `EntryService`. The legacy surface exists so older clients keep
//! working through the transition; its retirement is #129.

mod interface;

use interface::EntryService;
use std::sync::Arc;
use timetrack_core::JsonStore;
use timetrack_proto::{
    BUS_NAME, Entry2, OBJECT_PATH, Project2, ServiceError, Snapshot2, TYPED_API_VERSION, WireError,
};
use zbus::interface;
use zbus::object_server::SignalEmitter;

struct EntryIface(Arc<EntryService>);

struct Entry2Iface(Arc<EntryService>);

/// Convert a service refusal into a D-Bus error.
///
/// The typed error travels as JSON in the `Failed` message body, so the
/// D-Bus signatures never change and old clients still get a readable
/// message; new clients decode the body back into `ServiceError`.
fn to_dbus_err(error: ServiceError) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_failed_body())
}

type Wire = String;

fn encode<T: serde::Serialize>(value: &T) -> Wire {
    serde_json::to_string(value).expect("wire types are always serializable")
}

/// Announce a persisted mutation (#103). A failed emit must not fail the
/// mutation — it is already durable, and clients keep a slow fallback
/// poll that notices a missed signal — so the error is only logged.
async fn announce(emitter: &SignalEmitter<'_>) {
    // Both interfaces announce (#110): clients subscribe on whichever
    // one they speak, and both must hear about every mutation.
    if let Err(e) = EntryIface::changed(emitter).await {
        eprintln!("timetrack-service: could not emit Changed: {e}");
    }
    if let Err(e) = Entry2Iface::changed(emitter).await {
        eprintln!("timetrack-service: could not emit Changed (typed): {e}");
    }
}

/// Finish a mutating call that returns a wire value: encode it, and on
/// success announce the change before replying, so a client refreshing
/// on the signal can never read pre-mutation state.
async fn changed_wire<T: serde::Serialize>(
    emitter: &SignalEmitter<'_>,
    result: Result<T, ServiceError>,
) -> zbus::fdo::Result<Wire> {
    let out = result.map(|v| encode(&v)).map_err(to_dbus_err);
    if out.is_ok() {
        announce(emitter).await;
    }
    out
}

/// Finish a typed mutating call (#110): map the refusal to its named
/// error, and announce on success before replying, as the legacy
/// helpers do.
async fn finish2<T>(
    emitter: &SignalEmitter<'_>,
    result: Result<T, ServiceError>,
) -> Result<T, WireError> {
    let out = result.map_err(WireError::from);
    if out.is_ok() {
        announce(emitter).await;
    }
    out
}

/// The same for mutating calls that return nothing.
async fn changed_unit(
    emitter: &SignalEmitter<'_>,
    result: Result<(), ServiceError>,
) -> zbus::fdo::Result<()> {
    let out = result.map_err(to_dbus_err);
    if out.is_ok() {
        announce(emitter).await;
    }
    out
}

#[interface(name = "org.sequ.timetrack.Entries")]
impl EntryIface {
    /// The store changed (#103): emitted after a mutation has been
    /// persisted, so idle clients refresh on the signal instead of
    /// polling full snapshots. Carries no payload — the snapshot stays
    /// the one consistent read — and clients keep a slow fallback poll,
    /// so an old client ignoring it (or a missed signal) only costs
    /// freshness, never correctness.
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// Everything a client needs to draw its first frame.
    #[zbus(name = "Snapshot")]
    fn snapshot(&self) -> Wire {
        encode(&self.0.snapshot())
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    /// Method 1: an explicit start and end.
    #[zbus(name = "Add")]
    async fn add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(
            &emitter,
            self.0.add(project_id, description, started_at, ended_at),
        )
        .await
    }

    /// Method 2: a duration ending now.
    #[zbus(name = "AddDuration")]
    async fn add_duration(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(
            &emitter,
            self.0.add_duration(project_id, description, duration_ms),
        )
        .await
    }

    /// Method 3: a duration ending at a given instant.
    #[zbus(name = "AddDurationEnding")]
    async fn add_duration_ending(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(
            &emitter,
            self.0
                .add_duration_ending(project_id, description, duration_ms, ended_at),
        )
        .await
    }

    /// Method 4: one of the quick-add buckets, ending now.
    #[zbus(name = "QuickAdd")]
    async fn quick_add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        duration_ms: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.quick_add(project_id, duration_ms)).await
    }

    // --- editing ---

    /// Shorten by moving an endpoint. Never deletes (REQUIREMENTS §5).
    #[zbus(name = "SetTimes")]
    async fn set_times(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.set_times(id, started_at, ended_at)).await
    }

    /// Edit an entry's description.
    #[zbus(name = "SetText")]
    async fn set_text(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        description: &str,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.set_text(id, description)).await
    }

    /// Reassign an entry to another project.
    #[zbus(name = "SetProject")]
    async fn set_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        project_id: &str,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.set_project(id, project_id)).await
    }

    /// Undo exactly the entry a quick-add created.
    #[zbus(name = "UndoQuickAdd")]
    async fn undo_quick_add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> zbus::fdo::Result<()> {
        changed_unit(&emitter, self.0.undo_quick_add(id)).await
    }

    /// Explicit deletion.
    #[zbus(name = "DeleteEntry")]
    async fn delete_entry(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> zbus::fdo::Result<()> {
        changed_unit(&emitter, self.0.delete_entry(id)).await
    }

    #[zbus(name = "Split")]
    async fn split(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        at_ms: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.split(id, at_ms)).await
    }

    #[zbus(name = "Merge")]
    async fn merge(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ids: Vec<String>,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.merge(&ids)).await
    }

    // --- projects ---

    #[zbus(name = "AddProject")]
    async fn add_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: &str,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.add_project(name)).await
    }

    #[zbus(name = "UpdateProject")]
    async fn update_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        name: &str,
        colour: i64,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.update_project(id, name, colour)).await
    }

    #[zbus(name = "SetArchived")]
    async fn set_archived(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        archived: bool,
    ) -> zbus::fdo::Result<Wire> {
        changed_wire(&emitter, self.0.set_archived(id, archived)).await
    }

    #[zbus(name = "DeleteProject")]
    async fn delete_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> zbus::fdo::Result<()> {
        changed_unit(&emitter, self.0.delete_project(id)).await
    }

    /// CSV export (REQUIREMENTS §10). `scope` is `week`, `month` or `all`;
    /// the reply is the CSV text (JSON-encoded on the wire like the rest).
    #[zbus(name = "ExportCsv")]
    fn export_csv(&self, scope: &str) -> zbus::fdo::Result<Wire> {
        self.0
            .export_csv(scope)
            .map(|csv| encode(&csv))
            .map_err(to_dbus_err)
    }

    /// CSV export anchored at `anchor_ms` (REQUIREMENTS §10). `scope` is
    /// `week`, `month` or `all`; the anchor selects which week or month and
    /// is ignored by `all`. The old `ExportCsv` stays as the now-anchored
    /// wrapper, so old clients keep current-period behaviour.
    #[zbus(name = "ExportCsvAt")]
    fn export_csv_at(&self, scope: &str, anchor_ms: i64) -> zbus::fdo::Result<Wire> {
        self.0
            .export_csv_at(scope, anchor_ms)
            .map(|csv| encode(&csv))
            .map_err(to_dbus_err)
    }
}

/// The typed interface (#110, ADR 0012): the same operations over the
/// same core, with struct arguments and replies and [`WireError`]
/// refusals, so introspection shows the real contract. Method names
/// match the legacy interface deliberately — the interface name is the
/// version, the vocabulary does not change.
#[interface(name = "org.sequ.timetrack.Entries2")]
impl Entry2Iface {
    /// The typed API revision, probed by clients at connect.
    #[zbus(name = "ApiVersion")]
    fn api_version(&self) -> u32 {
        TYPED_API_VERSION
    }

    /// See the legacy interface's `Changed`: emitted on both.
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(name = "Snapshot")]
    fn snapshot(&self) -> Snapshot2 {
        Snapshot2::from(&self.0.snapshot())
    }

    #[zbus(name = "Add")]
    async fn add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0
                .add(project_id, description, started_at, ended_at)
                .map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "AddDuration")]
    async fn add_duration(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0
                .add_duration(project_id, description, duration_ms)
                .map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "AddDurationEnding")]
    async fn add_duration_ending(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0
                .add_duration_ending(project_id, description, duration_ms, ended_at)
                .map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "QuickAdd")]
    async fn quick_add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        project_id: &str,
        duration_ms: i64,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0
                .quick_add(project_id, duration_ms)
                .map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "SetTimes")]
    async fn set_times(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0
                .set_times(id, started_at, ended_at)
                .map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "SetText")]
    async fn set_text(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        description: &str,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0.set_text(id, description).map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "SetProject")]
    async fn set_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        project_id: &str,
    ) -> Result<Entry2, WireError> {
        finish2(
            &emitter,
            self.0.set_project(id, project_id).map(|e| Entry2::from(&e)),
        )
        .await
    }

    #[zbus(name = "UndoQuickAdd")]
    async fn undo_quick_add(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> Result<(), WireError> {
        finish2(&emitter, self.0.undo_quick_add(id)).await
    }

    #[zbus(name = "DeleteEntry")]
    async fn delete_entry(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> Result<(), WireError> {
        finish2(&emitter, self.0.delete_entry(id)).await
    }

    #[zbus(name = "Split")]
    async fn split(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        at_ms: i64,
    ) -> Result<(Entry2, Entry2), WireError> {
        finish2(
            &emitter,
            self.0
                .split(id, at_ms)
                .map(|(a, b)| (Entry2::from(&a), Entry2::from(&b))),
        )
        .await
    }

    #[zbus(name = "Merge")]
    async fn merge(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ids: Vec<String>,
    ) -> Result<Entry2, WireError> {
        finish2(&emitter, self.0.merge(&ids).map(|e| Entry2::from(&e))).await
    }

    #[zbus(name = "AddProject")]
    async fn add_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: &str,
    ) -> Result<Project2, WireError> {
        finish2(
            &emitter,
            self.0.add_project(name).map(|p| Project2::from(&p)),
        )
        .await
    }

    #[zbus(name = "UpdateProject")]
    async fn update_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        name: &str,
        colour: i64,
    ) -> Result<Project2, WireError> {
        finish2(
            &emitter,
            self.0
                .update_project(id, name, colour)
                .map(|p| Project2::from(&p)),
        )
        .await
    }

    #[zbus(name = "SetArchived")]
    async fn set_archived(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        archived: bool,
    ) -> Result<Project2, WireError> {
        finish2(
            &emitter,
            self.0
                .set_archived(id, archived)
                .map(|p| Project2::from(&p)),
        )
        .await
    }

    #[zbus(name = "DeleteProject")]
    async fn delete_project(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
    ) -> Result<(), WireError> {
        finish2(&emitter, self.0.delete_project(id)).await
    }

    #[zbus(name = "ExportCsv")]
    fn export_csv(&self, scope: &str) -> Result<String, WireError> {
        self.0.export_csv(scope).map_err(WireError::from)
    }

    #[zbus(name = "ExportCsvAt")]
    fn export_csv_at(&self, scope: &str, anchor_ms: i64) -> Result<String, WireError> {
        self.0
            .export_csv_at(scope, anchor_ms)
            .map_err(WireError::from)
    }
}

/// zbus uses the async-io backend (see the workspace Cargo.toml), so the
/// runtime is `async_io`, not tokio. A tokio main would leave no reactor for
/// zbus' internal executor.
fn main() -> anyhow::Result<()> {
    async_io::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let store = JsonStore::new(
        std::env::args_os()
            .nth(1)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(JsonStore::default_path),
    );
    let service = Arc::new(EntryService::new(store)?);

    // Both interfaces on the one object through the transition (#110,
    // ADR 0012): typed for new clients, JSON for older ones, one core
    // under both.
    let _dbus = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, EntryIface(Arc::clone(&service)))?
        .serve_at(OBJECT_PATH, Entry2Iface(service))?
        .build()
        .await?;

    // A plain, greppable line. Both clients wait for the name to appear on the
    // bus, so make activation observable for humans and scripts alike.
    println!("timetrack-service: listening on {BUS_NAME}");

    // Park forever. Ctrl-C kills the process outright, which is fine: every
    // mutation is persisted before it is acknowledged, so there is no state to
    // flush on the way out.
    std::future::pending::<()>().await;
    unreachable!()
}

#[cfg(test)]
mod tests {
    use timetrack_proto::{BUS_NAME, INTERFACE, OBJECT_PATH, ServiceError};

    #[test]
    fn constants_are_consistent() {
        assert_eq!(BUS_NAME, "org.sequ.timetrack");
        assert_eq!(OBJECT_PATH, "/org/sequ/timetrack");
        // The interface was renamed off ".Timer" when the stopwatch went; a
        // client still asking for the old name must fail loudly rather than
        // reach a half-removed API.
        assert_eq!(INTERFACE, "org.sequ.timetrack.Entries");
        assert_ne!(INTERFACE, "org.sequ.timetrack.Timer");
    }

    #[test]
    fn refusals_cross_the_bus_typed() {
        // What `to_dbus_err` puts on the wire must decode back in the
        // client, or the typed errors are fiction. The D-Bus name stays
        // `Failed` so signatures never change.
        for error in [
            ServiceError::Rule(timetrack_core::RuleError::NotQuickAdd("e1".into())),
            ServiceError::UnknownExportScope("bogus".into()),
            ServiceError::Storage("could not write /s: denied".into()),
        ] {
            match super::to_dbus_err(error.clone()) {
                zbus::fdo::Error::Failed(body) => {
                    assert_eq!(ServiceError::from_failed_body(&body), error)
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }
}
