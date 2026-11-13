/* service/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack service.
//!
//! Owns the store and serves it over the local socket (`socket.rs`,
//! §16/ADR 0013). Both front ends are clients: the GPUI GUI and the
//! CLI, which means tracked data outlives either one.
//!
//! The D-Bus interfaces this binary used to dual-serve (the original
//! JSON-in-string `Entries` and the typed `Entries2`, ADR 0012) were
//! retired with the socket transition (#129): the socket carries the
//! same typed contract on every desktop OS, and keeping a Linux-only
//! second wire alive was the cost the transition existed to remove.
//! The tray's *menu* is still D-Bus (StatusNotifier is a D-Bus
//! protocol); that is the tray's own surface, not this service's.

mod caldav;
mod interface;
mod platform;
mod socket;

use interface::EntryService;
use std::sync::Arc;
use timetrack_store::SqliteStore;

fn main() -> anyhow::Result<()> {
    // The argument still names the legacy JSON store, as the test rigs
    // and a decade of muscle memory expect; the SQLite database lives
    // beside it (`store.db`) and imports the JSON history on first
    // open (§17).
    let json_path = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(platform::default_store_path);
    let store = SqliteStore::open_at(&json_path)?;
    let service = Arc::new(EntryService::new(store)?);

    // Socket ownership is the single-instance test (§16): a live answer
    // on the socket means this process is a second copy (AppRun races,
    // a client's auto-spawn racing the launcher) -- it exits quietly,
    // as a second copy always has. Any other bind failure is fatal:
    // the socket is the only transport now, so there is nothing useful
    // left to serve without it.
    match socket::serve(Arc::clone(&service))? {
        socket::ServeOutcome::Listening(path) => {
            println!("timetrack-service: listening on {}", path.display());
        }
        socket::ServeOutcome::AlreadyRunning => {
            println!("timetrack-service: already running; this copy exits");
            return Ok(());
        }
    }

    // Park forever. Ctrl-C kills the process outright, which is fine:
    // every mutation is persisted before it is acknowledged, so there
    // is no state to flush on the way out.
    loop {
        std::thread::park();
    }
}
