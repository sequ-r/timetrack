/* service/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack service.
//!
//! Owns the running timer and serves it on the session bus. Both front ends
//! are clients: the GPUI GUI and the ratatui CLI, which means the timer keeps
//! running when either one exits.

mod interface;

use interface::TimerService;
use timetrack_core::JsonStore;
use timetrack_proto::{BUS_NAME, OBJECT_PATH};
use zbus::interface;

struct TimerIface(TimerService);

/// Convert service errors into a D-Bus error the clients can surface verbatim.
fn to_dbus_err(e: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

// The wire types are `serde` structs rather than native zvariant ones, so the
// boundary is a JSON string. That keeps the protocol versioned by the same
// definitions the clients already use, and means adding a field to Snapshot is
// not a D-Bus signature change.

// A snapshot serialised for transport.
type Wire = String;

fn encode<T: serde::Serialize>(value: &T) -> Wire {
    serde_json::to_string(value).expect("wire types are always serializable")
}

#[interface(name = "org.sequ.timetrack.Timer")]
impl TimerIface {
    /// Everything a client needs to draw its first frame.
    #[zbus(name = "Snapshot")]
    fn snapshot(&self) -> Wire {
        encode(&self.0.snapshot())
    }

    #[zbus(name = "Start")]
    fn start(&self, description: &str) -> zbus::fdo::Result<Wire> {
        self.0.start(description).map(|e| encode(&e)).map_err(to_dbus_err)
    }

    #[zbus(name = "Stop")]
    fn stop(&self) -> zbus::fdo::Result<Wire> {
        self.0.stop().map(|e| encode(&e)).map_err(to_dbus_err)
    }

    #[zbus(name = "Cancel")]
    fn cancel(&self) -> zbus::fdo::Result<Wire> {
        self.0.cancel().map(|e| encode(&e)).map_err(to_dbus_err)
    }

    #[zbus(name = "Remove")]
    fn remove(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0.remove(id).map_err(to_dbus_err)
    }

    #[zbus(name = "Rename")]
    fn rename(&self, id: &str, description: &str) -> zbus::fdo::Result<Wire> {
        self.0
            .rename(id, description)
            .map(|e| encode(&e))
            .map_err(to_dbus_err)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store = JsonStore::new(
        std::env::args_os()
            .nth(1)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(JsonStore::default_path),
    );
    let service = TimerService::new(store)?;
    let running = service
        .snapshot()
        .running
        .map(|e| e.description)
        .unwrap_or_default();

    let _dbus = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, TimerIface(service))?
        .build()
        .await?;

    // A plain, greppable line. Both clients wait for the name to appear on the
    // bus, so make activation observable for humans and scripts alike.
    if running.is_empty() {
        println!("timetrack-service: listening on {BUS_NAME} (idle)");
    } else {
        println!("timetrack-service: listening on {BUS_NAME} (running: {running})");
    }

    tokio::signal::ctrl_c().await?;
    println!("timetrack-service: shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use timetrack_proto::{BUS_NAME, INTERFACE, OBJECT_PATH};

    #[test]
    fn constants_are_consistent() {
        assert_eq!(BUS_NAME, "org.sequ.timetrack");
        assert_eq!(OBJECT_PATH, "/org/sequ/timetrack");
        assert_eq!(INTERFACE, "org.sequ.timetrack.Timer");
    }
}
