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
//! # Why the payloads are JSON strings
//!
//! Every method takes and returns a single `s` holding JSON. A hand-written
//! D-Bus signature per call would mean two definitions of every type -- one in
//! the interface, one in the client -- and they would drift. With JSON the
//! serde structs in `timetrack-proto` are the single definition, so adding a
//! field to `Snapshot` is not a signature change.

mod interface;

use interface::EntryService;
use timetrack_core::JsonStore;
use timetrack_proto::{BUS_NAME, OBJECT_PATH, ServiceError};
use zbus::interface;

struct EntryIface(EntryService);

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

#[interface(name = "org.sequ.timetrack.Entries")]
impl EntryIface {
    /// Everything a client needs to draw its first frame.
    #[zbus(name = "Snapshot")]
    fn snapshot(&self) -> Wire {
        encode(&self.0.snapshot())
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    /// Method 1: an explicit start and end.
    #[zbus(name = "Add")]
    fn add(
        &self,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> zbus::fdo::Result<Wire> {
        self.0
            .add(project_id, description, started_at, ended_at)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Method 2: a duration ending now.
    #[zbus(name = "AddDuration")]
    fn add_duration(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> zbus::fdo::Result<Wire> {
        self.0
            .add_duration(project_id, description, duration_ms)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Method 3: a duration ending at a given instant.
    #[zbus(name = "AddDurationEnding")]
    fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> zbus::fdo::Result<Wire> {
        self.0
            .add_duration_ending(project_id, description, duration_ms, ended_at)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Method 4: one of the quick-add buckets, ending now.
    #[zbus(name = "QuickAdd")]
    fn quick_add(&self, project_id: &str, duration_ms: i64) -> zbus::fdo::Result<Wire> {
        self.0
            .quick_add(project_id, duration_ms)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    // --- editing ---

    /// Shorten by moving an endpoint. Never deletes (REQUIREMENTS §5).
    #[zbus(name = "SetTimes")]
    fn set_times(&self, id: &str, started_at: i64, ended_at: i64) -> zbus::fdo::Result<Wire> {
        self.0
            .set_times(id, started_at, ended_at)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Edit an entry's description.
    #[zbus(name = "SetText")]
    fn set_text(&self, id: &str, description: &str) -> zbus::fdo::Result<Wire> {
        self.0
            .set_text(id, description)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Reassign an entry to another project.
    #[zbus(name = "SetProject")]
    fn set_project(&self, id: &str, project_id: &str) -> zbus::fdo::Result<Wire> {
        self.0
            .set_project(id, project_id)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }

    /// Undo exactly the entry a quick-add created.
    #[zbus(name = "UndoQuickAdd")]
    fn undo_quick_add(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0.undo_quick_add(id).map_err(to_dbus_err)
    }

    /// Explicit deletion.
    #[zbus(name = "DeleteEntry")]
    fn delete_entry(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0.delete_entry(id).map_err(to_dbus_err)
    }

    #[zbus(name = "Split")]
    fn split(&self, id: &str, at_ms: i64) -> zbus::fdo::Result<Wire> {
        let (a, b) = self.0.split(id, at_ms).map_err(to_dbus_err)?;
        Ok(encode(&(a, b)))
    }

    #[zbus(name = "Merge")]
    fn merge(&self, ids: Vec<String>) -> zbus::fdo::Result<Wire> {
        self.0.merge(&ids).map(|e| encode(&e)).map_err(to_dbus_err)
    }

    // --- projects ---

    #[zbus(name = "AddProject")]
    fn add_project(&self, name: &str) -> zbus::fdo::Result<Wire> {
        self.0
            .add_project(name)
            .map(|p| encode(&p))
            .map_err(to_dbus_err)
    }

    #[zbus(name = "UpdateProject")]
    fn update_project(&self, id: &str, name: &str, colour: i64) -> zbus::fdo::Result<Wire> {
        self.0
            .update_project(id, name, colour)
            .map(|p| encode(&p))
            .map_err(to_dbus_err)
    }

    #[zbus(name = "SetArchived")]
    fn set_archived(&self, id: &str, archived: bool) -> zbus::fdo::Result<Wire> {
        self.0
            .set_archived(id, archived)
            .map(|p| encode(&p))
            .map_err(to_dbus_err)
    }

    #[zbus(name = "DeleteProject")]
    fn delete_project(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0.delete_project(id).map_err(to_dbus_err)
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
}

/// zbus uses the async-io backend (see the workspace Cargo.toml), so the
/// runtime is `async_io`, not tokio. `tokio::main` would leave no reactor for
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
    let service = EntryService::new(store)?;

    let _dbus = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, EntryIface(service))?
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
