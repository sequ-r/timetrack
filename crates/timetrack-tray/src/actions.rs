/* tray/actions.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Quick-add action bookkeeping: labels, outcome reporting, and the
//! per-session undo records the menu workers maintain.
//!
//! Plus the neutral dispatch every backend invokes: one [`TrayAction`]
//! per menu row, and [`dispatch`] holding exactly the worker bodies the
//! Linux ksni closures used to inline. Backends stay one-line calls.

use crate::ipc::{SharedClient, shared_client};
use crate::lock;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use timetrack_core::RuleError;
use timetrack_proto::{ClientError, ServiceError};

/// Label for a quick-add bucket row, in minutes like the GUI and CLI buckets
/// (`+60m`, not the compact `1h`, so the four rows read uniformly).
pub fn quick_add_label(duration_ms: i64) -> String {
    format!("+{}m", duration_ms / 60_000)
}

/// Record the outcome of a menu action, so the icon and the tooltip can say
/// what happened.
///
/// A failure used to reach the user only as a line on stderr, in a terminal
/// that may not be open: the menu did not change and the icon stayed
/// healthy, so a quick-add that failed looked exactly like one that worked.
///
/// The message is sticky until the next action succeeds -- unlike the
/// connection state, which the poll loop clears on the next good snapshot.
pub fn report_action(error: &Arc<Mutex<Option<String>>>, outcome: Option<&ClientError>) {
    *lock(error) = outcome.map(|e| e.to_string());
}

/// Remember the entry a quick-add created, so undo (#10) targets exactly the
/// id the service handed back rather than re-deriving it from the list.
pub fn record_quick_add(
    undo: &Arc<Mutex<HashMap<String, String>>>,
    project_id: &str,
    entry_id: &str,
) {
    lock(undo).insert(project_id.to_string(), entry_id.to_string());
}

/// Drop the undo record for `project_id`, but only if it still points at
/// `entry_id`: a newer quick-add may have moved the target while the undo
/// call was in flight, and finishing an undo must never clear a newer record.
pub fn forget_quick_add_if(
    undo: &Arc<Mutex<HashMap<String, String>>>,
    project_id: &str,
    entry_id: &str,
) {
    let mut guard = lock(undo);
    if guard.get(project_id).map(String::as_str) == Some(entry_id) {
        guard.remove(project_id);
    }
}
/// Whether a failed undo means the record is stale and must be dropped so
/// the row disables instead of failing forever.
///
/// A refusal naming the entry (`NotQuickAdd`, `UnknownEntry`) says the entry
/// is gone or was never a quick-add -- it was removed or edited elsewhere,
/// so no retry can succeed. Anything else (no service, transport) is
/// transient: the record stays and the next click retries.
pub fn undo_record_stale(result: &Result<(), ClientError>) -> bool {
    matches!(
        result,
        Err(ClientError::Service(ServiceError::Rule(
            RuleError::NotQuickAdd(_) | RuleError::UnknownEntry(_)
        )))
    )
}

/// One menu action, backend-neutral: every tray backend (ksni on Linux,
/// native menus on Windows/macOS in #155/#156) translates its row clicks
/// into these and calls [`dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// Quick-add `duration_ms` to `project_id` (tray-5, #9).
    QuickAdd {
        project_id: String,
        duration_ms: i64,
    },
    /// Undo this session's last quick-add to `project_id` (tray-6, #10).
    /// The entry id is read at dispatch time, so a quick-add that lands
    /// between menu build and click still undoes the latest entry.
    Undo { project_id: String },
    /// Delete `entry_id` behind its confirm label (tray-7, #11).
    DeleteEntry {
        entry_id: String,
        confirm_label: String,
    },
    /// Open the GUI (tray-10, #14).
    OpenGui,
    /// Quit the tray only, never the service (tray-10, #14).
    Quit,
}

/// What [`dispatch`] needs: the session undo map, the quit flag, the
/// shared service connection, and the sticky action-error slot. Backends
/// build one from their item state per click.
#[derive(Clone)]
pub struct DispatchContext {
    pub undo: Arc<Mutex<HashMap<String, String>>>,
    pub shutdown: Arc<AtomicBool>,
    pub client: SharedClient,
    pub action_error: Arc<Mutex<Option<String>>>,
}

impl DispatchContext {
    pub fn new(
        undo: Arc<Mutex<HashMap<String, String>>>,
        shutdown: Arc<AtomicBool>,
        client: SharedClient,
        action_error: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            undo,
            shutdown,
            client,
            action_error,
        }
    }
}

/// Run one menu action. Returns at once: service calls go out on worker
/// threads (the callers run on menu threads that must not block), while
/// Open and Quit act inline. Bodies are exactly the ksni-closure workers
/// this replaced, log lines included.
pub fn dispatch(action: TrayAction, ctx: &DispatchContext) {
    match action {
        TrayAction::QuickAdd {
            project_id,
            duration_ms,
        } => {
            // `activate` runs on the D-Bus thread and must not block:
            // hand the call to a worker thread and return. The worker
            // records the created id for undo below when it lands.
            let ms = duration_ms;
            let undo = ctx.undo.clone();
            let slot = ctx.client.clone();
            let failed = ctx.action_error.clone();
            std::thread::spawn(move || {
                let result: Result<timetrack_proto::EntryView, ClientError> =
                    async_io::block_on(async {
                        // The tray's own connection (#50): the click
                        // reuses it instead of opening a new one.
                        let client = shared_client(&slot).ok_or(ClientError::NoService)?;
                        client.quick_add(&project_id, ms).await
                    });
                match result {
                    Ok(entry) => {
                        record_quick_add(&undo, &project_id, &entry.id);
                        report_action(&failed, None);
                        println!(
                            "timetrack-tray: quick-added {} to {project_id} ({})",
                            entry.id,
                            quick_add_label(ms)
                        );
                    }
                    Err(e) => {
                        report_action(&failed, Some(&e));
                        eprintln!("timetrack-tray: quick add to {project_id} failed: {e}");
                    }
                }
            });
        }
        TrayAction::Undo { project_id } => {
            // Enabled only when this session quick-added to this project.
            // The click reads the current id at click time rather than
            // the one present at menu build, so a quick-add that lands
            // between build and click still undoes the latest entry.
            let Some(entry_id) = lock(&ctx.undo).get(&project_id).cloned() else {
                eprintln!("timetrack-tray: nothing to undo for {project_id}");
                return;
            };
            let undo = ctx.undo.clone();
            let slot = ctx.client.clone();
            let failed = ctx.action_error.clone();
            std::thread::spawn(move || {
                let result: Result<(), ClientError> = async_io::block_on(async {
                    // The tray's own connection (#50): the click reuses
                    // it instead of opening a new one.
                    let client = shared_client(&slot).ok_or(ClientError::NoService)?;
                    client.undo_quick_add(&entry_id).await
                });
                match &result {
                    Ok(()) => {
                        forget_quick_add_if(&undo, &project_id, &entry_id);
                        report_action(&failed, None);
                        println!("timetrack-tray: undid quick add {entry_id} in {project_id}");
                    }
                    Err(e) if undo_record_stale(&result) => {
                        // The entry is gone or was never a quick-add
                        // (removed or edited elsewhere): drop the record
                        // so the row disables instead of failing forever.
                        forget_quick_add_if(&undo, &project_id, &entry_id);
                        report_action(&failed, Some(e));
                        eprintln!("timetrack-tray: undo of {entry_id} refused ({e}); forgetting");
                    }
                    Err(e) => {
                        // Transient (no service, transport): keep the
                        // record so the next click retries.
                        report_action(&failed, Some(e));
                        eprintln!("timetrack-tray: undo of {entry_id} failed: {e}");
                    }
                }
            });
        }
        TrayAction::DeleteEntry {
            entry_id,
            confirm_label,
        } => {
            let slot = ctx.client.clone();
            let failed = ctx.action_error.clone();
            std::thread::spawn(move || {
                let result: Result<(), ClientError> = async_io::block_on(async {
                    // The tray's own connection (#50): the click reuses
                    // it instead of opening a new one.
                    let client = shared_client(&slot).ok_or(ClientError::NoService)?;
                    client.delete_entry(&entry_id).await
                });
                match result {
                    Ok(()) => {
                        report_action(&failed, None);
                        println!("timetrack-tray: deleted {entry_id} ({confirm_label})");
                    }
                    Err(e) => {
                        report_action(&failed, Some(&e));
                        eprintln!("timetrack-tray: delete of {entry_id} failed: {e}");
                    }
                }
            });
        }
        TrayAction::OpenGui => {
            // Opening the GUI runs the per-OS launch command, so the tray
            // and the launcher agree; quitting only flips the shutdown
            // flag -- it never touches the service.
            let argv = crate::platform::open_gui_command();
            let Some(program) = argv.first() else {
                eprintln!("timetrack-tray: desktop entry has no Exec line");
                return;
            };
            match std::process::Command::new(program).args(&argv[1..]).spawn() {
                Ok(_) => println!("timetrack-tray: opened {program}"),
                Err(e) => eprintln!("timetrack-tray: could not open {program}: {e}"),
            }
        }
        TrayAction::Quit => {
            // Ask the poll loop to exit. Touches nothing on the bus:
            // quitting stops the tray only, never the service.
            ctx.shutdown.store(true, Ordering::Relaxed);
        }
    }
}
