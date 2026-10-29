/* tray/ipc.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The tray's shared service connection (#50).

use std::sync::{Arc, Mutex};
use timetrack_proto::Client;

/// The tray's one bus connection, shared by the poll loop and the menu
/// workers (#50).
///
/// The poll loop publishes the live client here; a menu click clones it
/// and makes its call on the clone, so a burst of clicks multiplies
/// threads but never bus connections.
pub(crate) type SharedClient = Arc<Mutex<Option<Client>>>;

/// The tray's live connection, cloned for one call.
///
/// `None` means the tray holds no connection -- the service is not
/// running -- so a caller must not open one of its own.
pub(crate) fn shared_client(slot: &SharedClient) -> Option<Client> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Publish the tray's live connection, or clear it once that connection
/// dies, so the menu workers always find the current one.
pub(crate) fn set_shared_client(slot: &SharedClient, client: Option<Client>) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = client;
}
