/* gui/app/service.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The bridge to the service: the messages the UI receives, the commands it
//! sends, and the thread that owns the bus connection.
//!
//! **This module must not import gpui.** The bus connection belongs to the
//! service thread -- a dedicated `std::thread` with an async-io runtime of
//! its own -- and nothing bus-related may ever be driven from a gpui
//! future: gpui runs its futures on executor threads that have no tokio
//! context, and zbus' `spawn_blocking` panics there.

use super::REFRESH;
use super::dialogs::source::CredentialSave;
use super::export::picker::export_period_label;
use super::export::save::{
    default_export_path, default_export_range_path, export_anchor_for_save, export_range_period_id,
    export_range_suggested_name, export_suggested_name, save_export, save_range_export,
};
use super::status::{StatusKind, StatusMsg};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::time::{Duration, Instant};
use timetrack_proto::{ClientError, Snapshot, no_service_hint};

/// A message handed from the service thread to the UI thread.
///
/// Snapshots do not travel here: they have their own bounded channel (see
/// `spawn_service_thread`), because they are the one kind of message that
/// can be dropped without losing anything.
pub(super) enum Msg {
    Error(StatusMsg),
    /// A success worth showing (e.g. where an export was written). Snapshots
    /// never clear feedback, so this survives until the next action.
    Status(String),
    /// An export save completed: the footer status plus where the bytes went
    /// and whether the portal granted it, so the Export tab can label a
    /// chosen path differently from the direct fallback.
    ExportSaved {
        status: String,
        dest: String,
        via_portal: bool,
    },
    /// An action succeeded. Snapshots never clear feedback (they follow
    /// every action and would make errors unreadable), so success arrives as
    /// its own message: the view speaks the sentence it queued when the
    /// action was sent, replacing the previous error.
    ActionOk,
    /// The update check found a newer release with this machine's AppImage
    /// asset. The view announces it; nothing downloads until the user
    /// presses `U` twice.
    UpdateAvailable {
        version: String,
        url: String,
    },
    /// The update check ran and found nothing newer.
    UpdateCurrent,
    /// The download renamed over the running AppImage. The user relaunches
    /// to run it; nothing restarts itself.
    UpdateInstalled {
        version: String,
    },
    /// The install failed after the user confirmed it. An offline launch is
    /// silence, never this: only a confirmed install reports failure.
    UpdateFailed {
        reason: String,
    },
    /// The pending external suggestions changed (§19). Sent only on
    /// change, beside the snapshot cadence; a bus-only service simply
    /// never produces it and the section stays hidden.
    Suggestions(Vec<timetrack_proto::SuggestionView>),
    /// The registered external sources changed (#153). Sent only on
    /// change, beside the snapshot cadence, like the suggestions.
    Sources(Vec<timetrack_proto::SourceView>),
    /// Current folder-sync status (#176), updated on change and sync ops.
    SyncStatus(timetrack_proto::SyncStatusView),
    /// Unresolved folder-sync conflicts (#176), updated on change.
    SyncConflicts(Vec<timetrack_proto::SyncConflictView>),
    /// Active or newly rotated pairing code to display in the code dialog.
    SyncCode {
        code: String,
        rotated: bool,
    },
    /// Folder chosen by the OS folder picker for the open sync dialog.
    SyncFolderPicked(String),
}

/// What an export save needs: the typed scope plus the paths and labels
/// derived from it. Bundles the five `ExportCsv` fields so the click site,
/// the action and the service thread pass one value instead of five loose
/// strings (the scope stays typed end to end — nothing re-parses it).
#[derive(Debug, Clone)]
pub(super) struct ExportRequest {
    pub(super) scope: timetrack_core::ExportScope,
    /// Direct-save fallback path when no portal answers.
    pub(super) path: String,
    /// `None` is the old scope-only call; `Some` the scope-plus-anchor call.
    pub(super) anchor_ms: Option<i64>,
    pub(super) period_id: String,
    /// Period-stamped name prefilled in the save dialog.
    pub(super) suggested_name: String,
}

/// Build the request for one export button, from one anchor and one zone.
///
/// Everything the request names -- the period, the file name, the fallback
/// path -- is derived together, so the file cannot be called `...-W41.csv`
/// while holding a different week. The destination used to be computed at
/// render time and captured in the click closure, so a zone that changed
/// between the last paint and the click put the file in a directory named
/// for a different week.
pub(super) fn export_request(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    now: i64,
    tz: &timetrack_core::Tz,
) -> ExportRequest {
    ExportRequest {
        scope,
        path: default_export_path(scope, anchor_ms, tz),
        anchor_ms: export_anchor_for_save(scope, anchor_ms, now, tz),
        period_id: export_period_label(scope, anchor_ms, tz),
        suggested_name: export_suggested_name(scope, anchor_ms, tz),
    }
}

/// What an export fetch asks the service for (#133).
///
/// The range travels as two day instants, never as an `ExportScope`:
/// core deliberately has no `ExportScope::Range`, so the range keeps its
/// own request shape end to end instead of re-parsing a scope.
#[derive(Debug, Clone)]
pub(super) enum ExportCall {
    /// One of the fixed scopes, with the optional scope-plus-anchor call.
    Scoped(ExportRequest),
    /// A custom from/to day range: always the range call.
    Range(RangeExportRequest),
}

/// A custom from/to range export (#133): the two day instants plus the
/// paths and labels derived from them. The days are instants anywhere
/// inside the first/last local day; the service buckets them with its own
/// zone, exactly like the anchored call.
#[derive(Debug, Clone)]
pub(super) struct RangeExportRequest {
    pub(super) from_ms: i64,
    pub(super) to_ms: i64,
    /// Direct-save fallback path when no portal answers.
    pub(super) path: String,
    /// The `from..to` day pair the dialog name and the status line print.
    pub(super) period_id: String,
    /// Period-stamped name prefilled in the save dialog.
    pub(super) suggested_name: String,
}

/// Build the request for the range export button, from two day instants
/// and one zone.
///
/// Like [`export_request`], everything is derived together from one
/// snapshot and one zone at the click site, so a zone that changed between
/// the last paint and the click cannot split the period from the file.
pub(super) fn export_range_request(
    from_ms: i64,
    to_ms: i64,
    tz: &timetrack_core::Tz,
) -> RangeExportRequest {
    RangeExportRequest {
        from_ms,
        to_ms,
        path: default_export_range_path(from_ms, to_ms, tz),
        period_id: export_range_period_id(from_ms, to_ms, tz),
        suggested_name: export_range_suggested_name(from_ms, to_ms, tz),
    }
}

/// A command the UI wants performed. The service thread owns the bus
/// connection, so the UI cannot issue D-Bus calls itself.
#[derive(Debug, Clone)]
pub(super) enum Action {
    /// The service announced a change (#103): fetch a fresh snapshot now
    /// instead of waiting for the fallback tick. Sent by the signal
    /// thread, never by the view.
    Refresh,
    /// Method 4: one of the fixed buckets, ending now.
    QuickAdd { project: String, ms: i64 },
    /// Remove the entry a quick add created.
    UndoQuickAdd(String),
    /// Explicit deletion.
    DeleteEntry(String),
    /// Archive a project, keeping its history in totals.
    ArchiveProject(String),
    /// Reverse an archive, making the project selectable again.
    UnarchiveProject(String),
    /// Create a project.
    AddProject(String),
    /// Rename a project. Colour is untouched (-1 keep); recolouring has
    /// its own action below (#104).
    UpdateProject { id: String, name: String },
    /// Recolour a project (#104): `Some` sets `0xRRGGBB`, `None` clears
    /// (the service's -2 sentinel). Name is untouched (empty keep).
    RecolourProject { id: String, colour: Option<u32> },
    /// Delete a project (#104). The view confirmed first; the service
    /// still owns the guards (last project, has entries) and reports them
    /// typed.
    DeleteProject(String),
    /// Move an entry to another project (#104), from the edit dialog's
    /// project field.
    SetProject { id: String, project: String },
    /// Method 1: an explicit start and end (the manual-entry dialog).
    AddEntry {
        project: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    },
    /// Move an entry's endpoints (the dialog's edit mode). Never deletes.
    SetTimes {
        id: String,
        started_at: i64,
        ended_at: i64,
    },
    /// Edit an entry's description (the dialog's edit mode).
    SetText { id: String, description: String },
    /// Split one entry in two at a moment (REQUIREMENTS §8). Lossless: the
    /// halves cover exactly the original interval.
    SplitEntry { id: String, at_ms: i64 },
    /// Merge entries into their union (REQUIREMENTS §8). Overlapping entries
    /// collapse, so the total can shrink -- the view confirms with the amount
    /// before sending this.
    MergeEntries { ids: Vec<String> },
    /// Export entries as CSV (REQUIREMENTS §10). The service thread fetches
    /// the CSV over D-Bus, asks the system save dialog where to put it, and
    /// writes it there: `Scoped` carries `anchor_ms`, which selects which
    /// week or month when `Some` (the scope-plus-anchor call), while `None`
    /// keeps the old scope-only call for all time and current scopes so old
    /// services keep serving them. `Range` always takes the range call
    /// (`client.export_csv_range`). `suggested_name` is the period-stamped
    /// name prefilled in the dialog; `path` is the direct-save fallback
    /// when no portal answers. `period_id` feeds the saved status line.
    ExportCsv(ExportCall),
    /// Launch the service: spawn the sibling `timetrack-service`
    /// binary (§16 auto-spawn). The AppImage starts its bundled service
    /// on launch too; socket ownership makes the second copy exit.
    StartService,
    /// Download a confirmed update asset and rename it over the running
    /// AppImage (REQUIREMENTS: only the AppImage updates itself; host
    /// builds never send this). Like the export save, the download runs on
    /// its own thread and reports back over the channel.
    InstallUpdate { url: String, version: String },
    /// Fetch external sources now (§19). `None` refreshes every
    /// source (the suggestions header); `Some` refreshes one (its row).
    /// Creates no entries either way.
    RefreshSources { id: Option<String> },
    /// Confirm a suggestion into an entry on its source's project.
    ConfirmSuggestion(String),
    /// Dismiss a suggestion for good.
    DismissSuggestion(String),
    /// Register an external source (#153). The service validates and
    /// derives an empty label from the locator, as for the CLI. A
    /// credential set follows on the new source's id, here on the
    /// service thread, so CalDAV setup finishes in one save and the
    /// secret goes straight to the OS store (#149).
    AddSource {
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
        credential: CredentialSave,
    },
    /// Rewrite a source (#153): the dialog's full new state, replaced
    /// wholesale on the wire, with its credential change applied after.
    UpdateSource {
        id: String,
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
        credential: CredentialSave,
    },
    /// Delete a source, its pending suggestions, and its stored secret.
    /// Decided suggestions and confirmed entries stay (#153).
    RemoveSource(String),
    /// Open the OS folder chooser for the sync setup dialog (#176).
    PickSyncFolder,
    /// Enable folder sync or join an existing sync folder (#176).
    SyncEnable {
        folder: String,
        pairing_code: Option<timetrack_proto::SecretString>,
    },
    /// Run a pull-merge-publish folder sync round now (#176).
    SyncNow,
    /// Pull-merge active peers ahead of publishing after falling behind (#176).
    SyncRepair,
    /// Stop folder sync and optionally delete this device's sync file (#176).
    SyncUnpair { delete_file: bool },
    /// Fetch the active pairing code to display to the user (#176).
    SyncShowCode,
    /// Rotate the 256-bit folder-sync key and show the new pairing code (#176).
    SyncRotateKey,
    /// Re-save a losing conflict record as a fresh local edit (#176).
    SyncRestoreConflict(String),
}

/// What happened to a snapshot offered to the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Offered {
    /// It is queued.
    Queued,
    /// The view is behind, so the snapshot was dropped.
    Dropped,
    /// The window is gone.
    Gone,
}

/// How long the service thread may block waiting for a command before the
/// next refresh falls due.
///
/// The thread waits on the action channel with this as the timeout, so a
/// command wakes it at once and only an idle channel waits the tick out.
/// Sleeping the whole tick
/// unconditionally used to leave every click queueing behind whatever was
/// left of it, which read as the service itself being slow. No snapshot
/// yet (`None`) means one is due now.
pub(super) fn refresh_wait(last_refresh: Option<Instant>) -> Duration {
    match last_refresh {
        Some(at) => REFRESH.saturating_sub(at.elapsed()),
        None => Duration::ZERO,
    }
}

/// Offer a snapshot to the view.
///
/// The queue holds one snapshot. A view that stops draining -- a hidden
/// window, a long paint -- must not make the queue grow without bound, and
/// the snapshot waiting there is already stale, so a new one is dropped
/// rather than queued behind it. The next tick's arrives and the view
/// catches up.
pub(super) fn offer_snapshot(tx: &SyncSender<Snapshot>, s: Snapshot) -> Offered {
    match tx.try_send(s) {
        Ok(()) => Offered::Queued,
        Err(std::sync::mpsc::TrySendError::Full(_)) => Offered::Dropped,
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Offered::Gone,
    }
}

/// Spawn the service thread.
///
/// Returns `(snapshots_rx, messages_rx, actions_tx)`.
///
/// The two channels back to the view are different on purpose.
///
/// **Snapshots** are bounded. They arrive after actions and `Changed`
/// announcements (plus a slow fallback tick) and the view drains them
/// every 300 ms, so a view that stops draining -- a hidden window, a
/// long paint -- must not make the queue grow without bound. A stale
/// snapshot is worth nothing once a newer exists, so the queue holds one
/// and a newer one is dropped rather than queued behind it; the next
/// announcement's arrives and the view catches up.
///
/// **Messages** -- errors, statuses, the result of an action -- are
/// unbounded and never dropped. Those are the service's refusals and the
/// user's intentions, and losing one would be a lie.
pub(super) fn spawn_service_thread() -> (Receiver<Snapshot>, Receiver<Msg>, Sender<Action>) {
    let (snapshot_tx, snapshot_rx) = std::sync::mpsc::sync_channel(1);
    let (tx, rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    // The `Changed` listener (#103): its own bus connection on its own
    // thread, forwarding each announcement to the service thread as a
    // `Refresh` action, so the service thread's somebody-changed refresh
    // goes through exactly the path a click takes. Any failure here is
    // silence, like the update check below: the fallback snapshot tick
    // still covers a signalless service, just more slowly.
    {
        let action_tx = action_tx.clone();
        std::thread::spawn(move || {
            async_io::block_on(async move {
                let Ok(client) = timetrack_proto::Client::connect().await else {
                    return;
                };
                let Ok(mut stream) = client.changed_stream().await else {
                    return;
                };
                while stream.next().await.is_some() {
                    if action_tx.send(Action::Refresh).is_err() {
                        return;
                    }
                }
            });
        });
    }
    // The self-update check: one blocking HTTPS call on its own thread, at
    // startup only. It never touches the bus, so it shares nothing with the
    // service loop below except the message channel back to the view. Any
    // failure is silence -- an offline launch is not an update failure --
    // and host builds skip it entirely: only the AppImage updates itself.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            if !timetrack_update::updates_enabled() {
                return;
            }
            let current = timetrack_update::Version::current();
            match timetrack_update::check_for_update(&current) {
                Ok(Some(release)) => {
                    if let Some(asset) = release.asset_for(timetrack_update::current_arch()) {
                        let _ = tx.send(Msg::UpdateAvailable {
                            version: release.version.as_str().to_string(),
                            url: asset.url.clone(),
                        });
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Msg::UpdateCurrent);
                }
                Err(_) => {}
            }
        });
    }
    std::thread::spawn(move || {
        // zbus uses the async-io backend (see the workspace Cargo.toml for why
        // that is not optional), so the bus is driven with `async_io::block_on`
        // on this dedicated thread rather than from a gpui future. gpui's
        // executor threads have no zbus-compatible reactor, and zbus'
        // `spawn_blocking` would panic there.
        async_io::block_on(async move {
            // Wait for the service, but keep retrying: the user may start it
            // after the window opens, and the GUI should recover on its own.
            //
            // The reported message distinguishes "not there" from "there but
            // speaking a different interface". Those need different fixes, and
            // a version mismatch between the GUI and the service is otherwise
            // indistinguishable from the service being absent. The two cases
            // sort by error variant in `ClientError`, never by message text.
            let mut last_error: Option<StatusMsg> = None;
            let client = loop {
                match timetrack_proto::Client::connect().await {
                    Ok(c) => break c,
                    Err(e) => {
                        // At startup any bus failure presents as the service
                        // being unreachable; once connected, failures triage
                        // precisely by variant. The detail rides along in
                        // parens, as it always has.
                        let note = StatusMsg {
                            text: format!("{}\n\n({e})", no_service_hint()),
                            kind: StatusKind::NoService,
                        };
                        if last_error.as_ref() != Some(&note) {
                            last_error = Some(note.clone());
                            let _ = tx.send(Msg::Error(note));
                        }
                    }
                }
                // Poll briskly: this is a bus round trip to a local process,
                // and a short backoff means a service started after the window
                // is picked up almost immediately rather than seconds later.
                async_io::Timer::after(Duration::from_millis(500)).await;
            };

            // When the last snapshot was taken. `None` means "not yet", so
            // the first one is due at once and the first frame is never
            // blank for a whole tick.
            let mut last_refresh: Option<Instant> = None;
            // The launch smoke check (`scripts/gui-launch-smoke.sh`)
            // asserts on the line this gates (#157): the first snapshot
            // that round-trips proves the GUI reached the service.
            let mut connected_logged = false;
            // The suggestions last sent to the view: refetched beside
            // each snapshot and forwarded only on change, so an idle
            // window is not repainted with the same list every tick.
            let mut last_suggestions: Vec<timetrack_proto::SuggestionView> = Vec::new();
            // The sources last sent to the view (#153): same cadence,
            // same change gate.
            let mut last_sources: Vec<timetrack_proto::SourceView> = Vec::new();
            // Folder-sync status and unresolved conflicts (#176): same
            // cadence and change gate, plus immediate push on sync actions.
            let mut last_sync_status: Option<timetrack_proto::SyncStatusView> = None;
            let mut last_sync_conflicts: Vec<timetrack_proto::SyncConflictView> = Vec::new();
            loop {
                // Wait for a command, but never longer than until the
                // next refresh is due: a click reaches the service at
                // once instead of queueing behind the tick's remainder.
                // zbus reads
                // the socket on its own executor thread, so blocking here
                // does not stall the bus. A timeout folds into the drain
                // loop's "nothing waiting" case below, which falls
                // through to the refresh.
                let mut pending = match action_rx.recv_timeout(refresh_wait(last_refresh)) {
                    Ok(action) => Ok(action),
                    Err(RecvTimeoutError::Timeout) => Err(TryRecvError::Empty),
                    // The window is gone; nothing left to do.
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                // Drain queued commands first, then refresh once. Batching
                // them means one snapshot per burst, not per click.
                loop {
                    match pending {
                        // Nothing to do here: the snapshot fetch below
                        // the drain is the refresh this asks for.
                        Ok(Action::Refresh) => {}
                        Ok(Action::QuickAdd { project, ms }) => {
                            report(&tx, client.quick_add(&project, ms).await.map(|_| ()));
                        }
                        Ok(Action::UndoQuickAdd(id)) => {
                            report(&tx, client.undo_quick_add(&id).await.map(|_| ()));
                        }
                        Ok(Action::DeleteEntry(id)) => {
                            report(&tx, client.delete_entry(&id).await.map(|_| ()));
                        }
                        Ok(Action::ArchiveProject(id)) => {
                            report(&tx, client.set_archived(&id, true).await.map(|_| ()));
                        }
                        Ok(Action::UnarchiveProject(id)) => {
                            report(&tx, client.set_archived(&id, false).await.map(|_| ()));
                        }
                        Ok(Action::AddProject(name)) => {
                            report(&tx, client.add_project(&name).await.map(|_| ()));
                        }
                        Ok(Action::UpdateProject { id, name }) => {
                            // Colour keeps its value: -1 is the service's
                            // keep sentinel.
                            report(&tx, client.update_project(&id, &name, -1).await.map(|_| ()));
                        }
                        Ok(Action::RecolourProject { id, colour }) => {
                            // Name keeps its value (empty); -2 clears the
                            // colour, a value sets its low 24 bits.
                            let wire = colour.map(i64::from).unwrap_or(-2);
                            report(&tx, client.update_project(&id, "", wire).await.map(|_| ()));
                        }
                        Ok(Action::DeleteProject(id)) => {
                            report(&tx, client.delete_project(&id).await.map(|_| ()));
                        }
                        Ok(Action::SetProject { id, project }) => {
                            report(&tx, client.set_project(&id, &project).await.map(|_| ()));
                        }
                        Ok(Action::AddEntry {
                            project,
                            description,
                            started_at,
                            ended_at,
                        }) => {
                            report(
                                &tx,
                                client
                                    .add(&project, &description, started_at, ended_at)
                                    .await
                                    .map(|_| ()),
                            );
                        }
                        Ok(Action::SetTimes {
                            id,
                            started_at,
                            ended_at,
                        }) => {
                            report(
                                &tx,
                                client
                                    .set_times(&id, started_at, ended_at)
                                    .await
                                    .map(|_| ()),
                            );
                        }
                        Ok(Action::SetText { id, description }) => {
                            report(&tx, client.set_text(&id, &description).await.map(|_| ()));
                        }
                        Ok(Action::SplitEntry { id, at_ms }) => {
                            report(&tx, client.split(&id, at_ms).await.map(|_| ()));
                        }
                        Ok(Action::MergeEntries { ids }) => {
                            report(&tx, client.merge(&ids).await.map(|_| ()));
                        }
                        Ok(Action::ExportCsv(call)) => {
                            // The service refusal stays typed; a local write
                            // failure is a local note, not a service error.
                            // All time and current scopes travel over the old
                            // scope-only call (`None`), so old services keep
                            // serving them; only a chosen week or month needs
                            // the scope-plus-anchor call, which against an old
                            // service refuses with a version message rather
                            // than writing a wrong file. The range always
                            // takes the range call, which a service without
                            // it refuses the same typed way.
                            let fetched = match &call {
                                ExportCall::Scoped(req) => {
                                    let scope_str = req.scope.as_str().to_string();
                                    match req.anchor_ms {
                                        Some(at) => client.export_csv_at(&scope_str, at).await,
                                        None => client.export_csv(&scope_str).await,
                                    }
                                }
                                ExportCall::Range(req) => {
                                    client.export_csv_range(req.from_ms, req.to_ms).await
                                }
                            };
                            match fetched {
                                Ok(csv) => {
                                    // The save dialog is the user's, and it
                                    // can stay open for as long as they
                                    // like. Awaiting it here would stop this
                                    // thread from serving anything else --
                                    // no snapshot, no quick-add -- until they
                                    // answered, and the window would look
                                    // frozen over a frozen data set.
                                    //
                                    // So the dialog and the write happen on
                                    // their own thread, and the result comes
                                    // back over the same channel.
                                    let tx = tx.clone();
                                    std::thread::spawn(move || {
                                        async_io::block_on(async move {
                                            match call {
                                                ExportCall::Scoped(req) => {
                                                    save_export(tx, csv, req).await;
                                                }
                                                ExportCall::Range(req) => {
                                                    save_range_export(tx, csv, req).await;
                                                }
                                            }
                                        });
                                    });
                                }
                                Err(e) => {
                                    let _ = tx.send(Msg::Error(StatusMsg::from(&e)));
                                }
                            }
                        }
                        Ok(Action::RefreshSources { id }) => {
                            report(&tx, client.refresh_source(id.as_deref()).await.map(|_| ()));
                        }
                        Ok(Action::ConfirmSuggestion(id)) => {
                            report(&tx, client.confirm_suggestion(&id, None).await.map(|_| ()));
                        }
                        Ok(Action::DismissSuggestion(id)) => {
                            report(&tx, client.dismiss_suggestion(&id).await.map(|_| ()));
                        }
                        Ok(Action::AddSource {
                            kind,
                            locator,
                            project_id,
                            label,
                            credential,
                        }) => {
                            // The credential needs the new id, so the two
                            // calls chain here. If the second refuses
                            // (a locked OS store), the source stands and
                            // the refusal reports typed — the row's
                            // "no credential" badge shows the gap.
                            let outcome = match client
                                .add_source(&kind, &locator, project_id.as_deref(), &label)
                                .await
                            {
                                Ok(created) => {
                                    apply_credential(&client, &created.id, &credential).await
                                }
                                Err(e) => Err(e),
                            };
                            report(&tx, outcome);
                        }
                        Ok(Action::UpdateSource {
                            id,
                            kind,
                            locator,
                            project_id,
                            label,
                            credential,
                        }) => {
                            let outcome = match client
                                .update_source(&id, &kind, &locator, project_id.as_deref(), &label)
                                .await
                            {
                                Ok(_) => apply_credential(&client, &id, &credential).await,
                                Err(e) => Err(e),
                            };
                            report(&tx, outcome);
                        }
                        Ok(Action::RemoveSource(id)) => {
                            report(&tx, client.remove_source(&id).await.map(|_| ()));
                        }
                        Ok(Action::PickSyncFolder) => {
                            let tx = tx.clone();
                            std::thread::spawn(move || {
                                async_io::block_on(async move {
                                    match super::export::save::portal_pick_folder().await {
                                        super::export::save::PortalOutcome::Granted(path) => {
                                            let _ = tx.send(Msg::SyncFolderPicked(path));
                                        }
                                        super::export::save::PortalOutcome::Cancelled => {}
                                        super::export::save::PortalOutcome::Unavailable => {
                                            let _ = tx.send(Msg::Status(
                                                "folder picker unavailable — type the folder path"
                                                    .to_string(),
                                            ));
                                        }
                                    }
                                });
                            });
                        }
                        Ok(Action::SyncEnable {
                            folder,
                            pairing_code,
                        }) => {
                            match client
                                .sync_enable(&folder, pairing_code.as_ref().map(|s| s.expose()))
                                .await
                            {
                                Ok(res) => {
                                    if let Some(code) = res.pairing_code {
                                        let _ = tx.send(Msg::SyncCode {
                                            code,
                                            rotated: false,
                                        });
                                    }
                                    last_sync_status = Some(res.status.clone());
                                    let _ = tx.send(Msg::SyncStatus(res.status));
                                    let _ = tx.send(Msg::ActionOk);
                                    if !res.warnings.is_empty() {
                                        let cleaned: Vec<String> = res
                                            .warnings
                                            .iter()
                                            .map(|w| timetrack_core::sanitize_display(w))
                                            .collect();
                                        let _ = tx.send(Msg::Status(format!(
                                            "sync enabled ({})",
                                            cleaned.join("; ")
                                        )));
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Msg::Error(StatusMsg::from(&e)));
                                }
                            }
                        }
                        Ok(Action::SyncNow) => {
                            apply_sync_round(&client, &tx, &mut last_sync_status, false).await;
                        }
                        Ok(Action::SyncRepair) => {
                            apply_sync_round(&client, &tx, &mut last_sync_status, true).await;
                        }
                        Ok(Action::SyncUnpair { delete_file }) => {
                            report(&tx, client.sync_unpair(delete_file).await);
                        }
                        Ok(Action::SyncShowCode) => match client.sync_show_code().await {
                            Ok(code) => {
                                let _ = tx.send(Msg::SyncCode {
                                    code,
                                    rotated: false,
                                });
                                report(&tx, Ok(()));
                            }
                            Err(e) => report(&tx, Err(e)),
                        },
                        Ok(Action::SyncRotateKey) => match client.sync_rotate_key().await {
                            Ok(code) => {
                                let _ = tx.send(Msg::SyncCode {
                                    code,
                                    rotated: true,
                                });
                                report(&tx, Ok(()));
                            }
                            Err(e) => report(&tx, Err(e)),
                        },
                        Ok(Action::SyncRestoreConflict(id)) => {
                            report(&tx, client.sync_restore_conflict(&id).await);
                        }
                        Ok(Action::StartService) => {
                            // Spawn the sibling binary, then prove it by
                            // snapshot: the next refresh tick would find
                            // it anyway, this just answers the click now.
                            timetrack_proto::socket::try_spawn_service();
                            report(&tx, client.snapshot().await.map(|_| ()));
                        }
                        Ok(Action::InstallUpdate { url, version }) => {
                            // The download outlives the tick: it runs on its
                            // own thread (like the export save) and reports
                            // back over the channel, so snapshots and clicks
                            // keep flowing while the bytes arrive.
                            let tx = tx.clone();
                            std::thread::spawn(move || {
                                let outcome = match timetrack_update::app_image_path() {
                                    Some(path) => timetrack_update::install_with(&url, &path)
                                        .map(|_| Msg::UpdateInstalled {
                                            version: version.clone(),
                                        })
                                        .unwrap_or_else(|e| Msg::UpdateFailed {
                                            reason: e.to_string(),
                                        }),
                                    None => Msg::UpdateFailed {
                                        reason: timetrack_update::UpdateError::NotAppImage
                                            .to_string(),
                                    },
                                };
                                let _ = tx.send(outcome);
                            });
                        }
                        Err(TryRecvError::Empty) => break,
                        // The window is gone; nothing left to do.
                        Err(TryRecvError::Disconnected) => return,
                    }
                    pending = action_rx.try_recv();
                }
                match client.snapshot().await {
                    Ok(s) => {
                        if !connected_logged {
                            connected_logged = true;
                            eprintln!("timetrack-gui: connected to the service");
                        }
                        // Suggestions ride the same cadence (§19). A
                        // bus-only service refuses here every time; that
                        // is silence, not an error worth a footer.
                        if let Ok(fresh) = client.suggestions().await
                            && fresh != last_suggestions
                        {
                            last_suggestions = fresh.clone();
                            let _ = tx.send(Msg::Suggestions(fresh));
                        }
                        // Sources ride it too (#153), under the same gate.
                        if let Ok(fresh) = client.sources().await
                            && fresh != last_sources
                        {
                            last_sources = fresh.clone();
                            let _ = tx.send(Msg::Sources(fresh));
                        }
                        // Folder sync status and unresolved conflicts (#176).
                        if let Ok(fresh) = client.sync_status().await
                            && last_sync_status.as_ref() != Some(&fresh)
                        {
                            last_sync_status = Some(fresh.clone());
                            let _ = tx.send(Msg::SyncStatus(fresh));
                        }
                        if let Ok(fresh) = client.sync_conflicts(false).await
                            && fresh != last_sync_conflicts
                        {
                            last_sync_conflicts = fresh.clone();
                            let _ = tx.send(Msg::SyncConflicts(fresh));
                        }
                        match offer_snapshot(&snapshot_tx, s) {
                            Offered::Queued | Offered::Dropped => {}
                            // The window is gone: nothing left to do.
                            Offered::Gone => return,
                        }
                    }
                    Err(e) => {
                        if tx.send(Msg::Error(StatusMsg::from(&e))).is_err() {
                            return;
                        }
                    }
                }
                last_refresh = Some(Instant::now());
            }
        });
    });
    (snapshot_rx, rx, action_tx)
}

/// Format one completed sync or repair round for the footer status line (#176).
pub(super) fn sync_round_summary(verb: &str, res: &timetrack_proto::SyncNowView) -> String {
    let mut base = format!(
        "{verb}: {} peer{}, {} change{}",
        res.peers_read,
        if res.peers_read == 1 { "" } else { "s" },
        res.applied_changes,
        if res.applied_changes == 1 { "" } else { "s" },
    );
    if res.new_conflicts > 0 {
        base.push_str(&format!(
            ", {} conflict{}",
            res.new_conflicts,
            if res.new_conflicts == 1 { "" } else { "s" },
        ));
    }
    if !res.warnings.is_empty() {
        let cleaned: Vec<String> = res
            .warnings
            .iter()
            .map(|w| timetrack_core::sanitize_display(w))
            .collect();
        base.push_str(&format!(" ({})", cleaned.join("; ")));
    }
    base
}

async fn apply_sync_round(
    client: &timetrack_proto::Client,
    tx: &Sender<Msg>,
    last_sync_status: &mut Option<timetrack_proto::SyncStatusView>,
    repair: bool,
) {
    let (verb, outcome) = if repair {
        ("sync repaired", client.sync_repair().await)
    } else {
        ("synced", client.sync_now().await)
    };
    match outcome {
        Ok(res) => {
            *last_sync_status = Some(res.status.clone());
            let _ = tx.send(Msg::SyncStatus(res.status.clone()));
            let _ = tx.send(Msg::ActionOk);
            let _ = tx.send(Msg::Status(sync_round_summary(verb, &res)));
        }
        Err(e) => {
            let _ = tx.send(Msg::Error(StatusMsg::from(&e)));
        }
    }
}

/// Apply a source save's credential half (#153): the secret travels
/// as a redacted `SecretString` on the socket and lands in the OS
/// credential store service-side (#149) — SQLite never sees it.
async fn apply_credential(
    client: &timetrack_proto::Client,
    id: &str,
    credential: &CredentialSave,
) -> std::result::Result<(), ClientError> {
    match credential {
        CredentialSave::Keep => Ok(()),
        CredentialSave::Set { username, secret } => client
            .set_source_credential(id, username, secret.expose())
            .await
            .map(|_| ()),
        CredentialSave::Remove => client.remove_source_credential(id).await.map(|_| ()),
    }
}

/// Forward a command outcome to the UI, keeping the error typed.
///
/// The kind travels with the text so the view never has to recognise a
/// failure by sniffing its wording.
fn report(tx: &Sender<Msg>, outcome: std::result::Result<(), ClientError>) {
    match outcome {
        Ok(()) => {
            let _ = tx.send(Msg::ActionOk);
        }
        Err(e) => {
            let _ = tx.send(Msg::Error(StatusMsg::from(&e)));
        }
    }
}

/// The current time, in milliseconds since the epoch.
///
/// The GUI needs its own clock reading only to parse what the user typed
/// ("-90", "now", "09:30" are relative to here), exactly like the CLI does.
/// The service re-validates the absolute instants, so a skew between the two
/// clocks can mistime an entry but never store an invalid one.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the snapshot queue ---

    #[test]
    fn a_snapshot_is_dropped_rather_than_queued_behind_a_stale_one() {
        // The queue holds one. A view that stops draining must not make it
        // grow, so a newer snapshot is dropped rather than queued behind the
        // stale one -- and the next tick's takes its place.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        assert_eq!(offer_snapshot(&tx, Snapshot::default()), Offered::Queued);
        assert_eq!(offer_snapshot(&tx, Snapshot::default()), Offered::Dropped);
        assert!(
            rx.try_recv().is_ok(),
            "the one that got there is still there"
        );
        assert!(rx.try_recv().is_err(), "and it is the only one");
    }

    // --- the refresh wait ---

    #[test]
    fn the_refresh_wait_is_what_is_left_of_the_tick() {
        // The service thread blocks on the action channel for this long:
        // an action arriving wakes it at once, and only an idle channel
        // waits the tick out. No snapshot yet means one is due now, so
        // the first frame never waits a whole tick.
        assert_eq!(refresh_wait(None), Duration::ZERO);
        let fresh = refresh_wait(Some(std::time::Instant::now()));
        assert!(
            fresh <= REFRESH && fresh > REFRESH / 2,
            "just refreshed, so nearly a whole tick remains: {fresh:?}"
        );
        let stale = refresh_wait(Some(std::time::Instant::now() - REFRESH * 2));
        assert_eq!(stale, Duration::ZERO, "overdue refreshes do not wait");
    }

    #[test]
    fn a_source_action_never_debug_prints_its_secret() {
        // #153: the secret crosses this channel as a redacted
        // SecretString, so an action logged for any reason cannot
        // leak the CalDAV password.
        let action = Action::AddSource {
            kind: "caldav".into(),
            locator: "https://cal.test/u/".into(),
            project_id: None,
            label: String::new(),
            credential: CredentialSave::Set {
                username: "alice".into(),
                secret: timetrack_proto::SecretString::new("s3cr3t-153"),
            },
        };
        let debug = format!("{action:?}");
        assert!(!debug.contains("s3cr3t-153"), "{debug}");
        assert!(debug.contains("alice"), "{debug}");
    }

    #[test]
    fn a_sync_enable_action_never_debug_prints_its_pairing_code() {
        let action = Action::SyncEnable {
            folder: "/tmp/shared".into(),
            pairing_code: Some(timetrack_proto::SecretString::new("SECRET-CODE-999")),
        };
        let debug = format!("{action:?}");
        assert!(!debug.contains("SECRET-CODE-999"), "{debug}");
        assert!(debug.contains("/tmp/shared"), "{debug}");
    }

    #[test]
    fn sync_round_summary_formats_changes_conflicts_and_warnings() {
        let status = timetrack_proto::SyncStatusView {
            enabled: true,
            device_id: "0123456789abcdef".into(),
            folder: Some("/tmp/sync".into()),
            has_key: true,
            merged_through: 1_000,
            compacted_through: 0,
            last_sync_at: Some(1_000),
            last_error: None,
            needs_repair: false,
            unresolved_conflicts: 0,
            peers: Vec::new(),
        };
        let clean = timetrack_proto::SyncNowView {
            status: status.clone(),
            peers_read: 1,
            applied_changes: 0,
            new_conflicts: 0,
            compacted_tombstones: 0,
            warnings: Vec::new(),
        };
        assert_eq!(
            sync_round_summary("synced", &clean),
            "synced: 1 peer, 0 changes"
        );
        let busy = timetrack_proto::SyncNowView {
            status,
            peers_read: 2,
            applied_changes: 1,
            new_conflicts: 1,
            compacted_tombstones: 0,
            warnings: vec!["peer stale".into()],
        };
        assert_eq!(
            sync_round_summary("sync repaired", &busy),
            "sync repaired: 2 peers, 1 change, 1 conflict (peer stale)"
        );
    }

    #[test]
    fn a_closed_view_is_told_the_window_is_gone() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx);
        assert_eq!(offer_snapshot(&tx, Snapshot::default()), Offered::Gone);
    }
}
