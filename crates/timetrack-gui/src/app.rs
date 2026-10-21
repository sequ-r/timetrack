/* gui/app.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The main window, and the bridge to the service.
//!
//! GPUI is single-threaded and owns the UI, but zbus is async, so the two meet
//! across a thread boundary: a background thread owns an async-io runtime and
//! the bus connection and forwards snapshots over a channel, while the UI
//! drains that channel during `render`. The view holds no authoritative state
//! -- every pixel it draws comes from a `Snapshot` the service produced, which
//! is what keeps the GUI and the CLI consistent when both are open.
//!
//! # Home is the week total, and nothing is above it
//!
//! The largest element on the Home tab is the current ISO week's total, and it
//! is the first thing in the column. No clock, no greeting, no header
//! (REQUIREMENTS §7). The number is the answer the app exists to give, so it
//! gets the top of the screen and everything else is quieter.
//!
//! # Nothing is running
//!
//! There is no timer state here, because there is none in the model. The Home
//! tab's primary control is the 5/15/30/60 quick-add row.

use gpui::prelude::*;
use gpui::{
    AsyncApp, Context, ElementId, Entity, FocusHandle, InteractiveElement, IntoElement,
    MouseButton, Render, StatefulInteractiveElement, Styled, Window, WindowControlArea, div, px,
    rgb, rgba,
};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::{ClientError, ProjectView, Snapshot, no_service_hint, version_mismatch_hint};

/// How often the service is polled for a new snapshot.
const REFRESH: Duration = Duration::from_millis(1000);

/// A snapshot handed from the service thread to the UI thread.
enum Msg {
    Snapshot(Box<Snapshot>),
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
    /// An action succeeded. Snapshots never clear feedback (they arrive
    /// every second and would make errors unreadable), so success arrives as
    /// its own message that dismisses the previous error.
    ActionOk,
}

/// What an export save needs: the typed scope plus the paths and labels
/// derived from it. Bundles the five `ExportCsv` fields so the click site,
/// the action and the service thread pass one value instead of five loose
/// strings (the scope stays typed end to end — nothing re-parses it).
#[derive(Debug, Clone)]
struct ExportRequest {
    scope: timetrack_core::ExportScope,
    /// Direct-save fallback path when no portal answers.
    path: String,
    /// `None` is the old scope-only call; `Some` the scope-plus-anchor call.
    anchor_ms: Option<i64>,
    period_id: String,
    /// Period-stamped name prefilled in the save dialog.
    suggested_name: String,
}

/// Build the request for one export button, from one anchor and one zone.
///
/// Everything the request names -- the period, the file name, the fallback
/// path -- is derived together, so the file cannot be called `...-W41.csv`
/// while holding a different week. The destination used to be computed at
/// render time and captured in the click closure, so a zone that changed
/// between the last paint and the click put the file in a directory named
/// for a different week.
fn export_request(
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

/// A command the UI wants performed. The service thread owns the bus
/// connection, so the UI cannot issue D-Bus calls itself.
#[derive(Debug, Clone)]
enum Action {
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
    /// Rename a project. Colour is untouched: the GUI names projects, the
    /// CLI recolours them (§11).
    UpdateProject { id: String, name: String },
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
    /// writes it there: `anchor_ms` selects which week or month when `Some`
    /// (the scope-plus-anchor call), while `None` keeps the old scope-only
    /// call for all time and current scopes so old services keep serving
    /// them. `suggested_name` is the period-stamped name prefilled in the
    /// dialog; `path` is the direct-save fallback when no portal answers.
    /// `period_id` feeds the saved status line.
    ExportCsv(ExportRequest),
    /// Launch the service. From a host build this asks the session bus to
    /// activate it, which works when a D-Bus activation file is installed;
    /// the AppImage starts its bundled service on launch, so the bus
    /// already owns the name there.
    StartService,
}

/// Spawn the service thread. Returns `(messages_rx, actions_tx)`.
fn spawn_service_thread() -> (Receiver<Msg>, Sender<Action>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
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

            loop {
                // Drain queued commands first, then refresh once. Batching
                // them means one bus round trip per tick, not per click.
                loop {
                    match action_rx.try_recv() {
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
                        Ok(Action::ExportCsv(req)) => {
                            // The service refusal stays typed; a local write
                            // failure is a local note, not a service error.
                            // All time and current scopes travel over the old
                            // scope-only call (`None`), so old services keep
                            // serving them; only a chosen week or month needs
                            // the scope-plus-anchor call, which against an old
                            // service refuses with a version message rather
                            // than writing a wrong file.
                            let scope_str = req.scope.as_str().to_string();
                            let fetched = match req.anchor_ms {
                                Some(at) => client.export_csv_at(&scope_str, at).await,
                                None => client.export_csv(&scope_str).await,
                            };
                            match fetched {
                                Ok(csv) => {
                                    // The dialog opens after the fetch, so a
                                    // service refusal never prompts for a
                                    // location. Cancel is a quiet status that
                                    // writes nothing; no portal falls back to
                                    // the direct home-directory write.
                                    let outcome = portal_save_file(&req.suggested_name).await;
                                    let via_portal = matches!(outcome, PortalOutcome::Granted(_));
                                    match resolve_export_destination(outcome, &req.path) {
                                        Some(dest) => {
                                            let _ = tx.send(export_write_msg(
                                                &csv,
                                                &dest,
                                                req.scope,
                                                &req.period_id,
                                                via_portal,
                                            ));
                                        }
                                        None => {
                                            let _ = tx.send(export_cancel_msg());
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Msg::Error(StatusMsg::from(&e)));
                                }
                            }
                        }
                        Ok(Action::StartService) => {
                            // Calling a method is what triggers D-Bus
                            // activation; `Client::connect` just asks for a
                            // proxy, so make a real call to force the bus to
                            // start the service if it can.
                            report(&tx, client.snapshot().await.map(|_| ()));
                        }
                        Err(TryRecvError::Empty) => break,
                        // The window is gone; nothing left to do.
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                match client.snapshot().await {
                    Ok(s) => {
                        if tx.send(Msg::Snapshot(Box::new(s))).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        if tx.send(Msg::Error(StatusMsg::from(&e))).is_err() {
                            return;
                        }
                    }
                }
                async_io::Timer::after(REFRESH).await;
            }
        });
    });
    (rx, action_tx)
}

/// What kind of problem a status line reports.
///
/// The kind drives behaviour -- a missing service offers to start it, and
/// only connection notices clear on recovery -- so it travels with the text
/// rather than being re-parsed out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    /// Nothing owns the service name.
    NoService,
    /// The service answered unknown-interface or unknown-method.
    VersionMismatch,
    /// The service refused an action; the text is its own wording.
    Service,
    /// Local UI notes and bus-level failures, not service refusals.
    Local,
}

/// The footer status line: what to show, and what kind of problem it is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusMsg {
    text: String,
    kind: StatusKind,
}

impl StatusMsg {
    /// A local UI note or bus-level failure: shown as-is, never offered the
    /// start-service action, and dismissed by the next action's success.
    fn local(text: impl Into<String>) -> Self {
        StatusMsg {
            text: text.into(),
            kind: StatusKind::Local,
        }
    }
}

impl From<&ClientError> for StatusMsg {
    /// Map a call failure to footer text without matching on message text:
    /// the variant decides both the wording and the kind.
    fn from(error: &ClientError) -> Self {
        match error {
            ClientError::NoService => StatusMsg {
                text: no_service_hint(),
                kind: StatusKind::NoService,
            },
            ClientError::VersionMismatch { .. } => StatusMsg {
                text: version_mismatch_hint("GUI"),
                kind: StatusKind::VersionMismatch,
            },
            ClientError::Service(service) => StatusMsg {
                text: service.to_string(),
                kind: StatusKind::Service,
            },
            ClientError::Transport(detail) => StatusMsg {
                text: detail.clone(),
                kind: StatusKind::Local,
            },
        }
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
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The snapshot's zone: prefer the name, fall back to the fixed offset for
/// old services that send no name.
fn snapshot_tz(snap: &timetrack_proto::Snapshot) -> timetrack_core::Tz {
    timetrack_core::Tz::from_snapshot(&snap.tz, snap.local_offset_ms)
}

/// Which field of the entry dialog receives keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogField {
    Description,
    Start,
    End,
}

impl DialogField {
    fn next(self) -> Self {
        match self {
            DialogField::Description => DialogField::Start,
            DialogField::Start => DialogField::End,
            DialogField::End => DialogField::Description,
        }
    }

    fn prev(self) -> Self {
        match self {
            DialogField::Description => DialogField::End,
            DialogField::Start => DialogField::Description,
            DialogField::End => DialogField::Start,
        }
    }
}

/// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
///
/// Method 2 is method 1 with "end = now" pre-filled, so a new dialog starts
/// with `start = "-60"`, `end = "now"` and the user edits from there. In edit
/// mode the times start empty, meaning "keep the current endpoint" — the
/// `keep_*` labels say what that is — which is the dialog form of `shorten`:
/// unspecified endpoints are left alone and the entry is always kept.
///
/// Plain keystroke editing (type to append, backspace to delete) rather than
/// a toolkit text field: the window already routes every key through one
/// focused root, and three small fields do not need IME or cursor movement.
/// Everything here is pure state over strings, so the whole interaction is
/// unit-testable without a window.
#[derive(Debug, Clone)]
struct EntryDialog {
    /// `None` for a new entry; `Some(id)` when editing that entry.
    edit_id: Option<String>,
    /// The project a new entry goes to, fixed when the dialog opens.
    project_id: String,
    project_name: String,
    description: String,
    start: String,
    end: String,
    /// Edit mode only: the current endpoints, for the "keep" fallback and
    /// the labels. New mode leaves these at zero and never reads them.
    orig_start: i64,
    orig_end: i64,
    orig_description: String,
    field: DialogField,
    error: Option<String>,
}

impl EntryDialog {
    /// A new-entry dialog for `project`, with "end = now" pre-filled.
    fn new(project_id: String, project_name: String) -> Self {
        EntryDialog {
            edit_id: None,
            project_id,
            project_name,
            description: String::new(),
            start: "-60".to_string(),
            end: "now".to_string(),
            orig_start: 0,
            orig_end: 0,
            orig_description: String::new(),
            field: DialogField::Description,
            error: None,
        }
    }

    /// An edit dialog for the entry with these current values. Times start
    /// empty, meaning "keep the current endpoint".
    fn edit(
        id: String,
        project_name: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    ) -> Self {
        EntryDialog {
            edit_id: Some(id),
            project_id: String::new(),
            project_name,
            description: description.clone(),
            start: String::new(),
            end: String::new(),
            orig_start: started_at,
            orig_end: ended_at,
            orig_description: description,
            field: DialogField::Description,
            error: None,
        }
    }

    /// Label for the start row: the hint in edit mode says what empty keeps.
    fn start_hint(&self, tz: &timetrack_core::Tz) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_start, tz)
            ),
        }
    }

    /// Label for the end row, as above.
    fn end_hint(&self, tz: &timetrack_core::Tz) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_end, tz)
            ),
        }
    }

    /// Append typed text to the focused field. Any edit clears a stale error:
    /// the message described the text as it was, not as it is now.
    fn type_text(&mut self, ch: &str) {
        let target = match self.field {
            DialogField::Description => &mut self.description,
            DialogField::Start => &mut self.start,
            DialogField::End => &mut self.end,
        };
        target.push_str(ch);
        self.error = None;
    }

    /// Delete the last character of the focused field.
    fn backspace(&mut self) {
        let target = match self.field {
            DialogField::Description => &mut self.description,
            DialogField::Start => &mut self.start,
            DialogField::End => &mut self.end,
        };
        target.pop();
        self.error = None;
    }
}

/// What saving a dialog means, resolved purely from its strings.
///
/// Pure over the dialog plus an explicit clock and offset, so the whole
/// save path — parsing, the end-before-start refusal, the keep-fallback — is
/// testable without a view or a service.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SaveOutcome {
    /// Create `[started_at, ended_at]` on `project_id` with `description`.
    Create {
        project_id: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    },
    /// Move `id`'s endpoints, and rewrite the description when present.
    Update {
        id: String,
        started_at: i64,
        ended_at: i64,
        description: Option<String>,
    },
    /// Nothing changed; close without calling the service.
    NoChange,
    /// Keep the dialog open and show this instead.
    Invalid(String),
}

fn resolve_save(dlg: &EntryDialog, now: i64, tz: &timetrack_core::Tz) -> SaveOutcome {
    match &dlg.edit_id {
        None => {
            if dlg.start.trim().is_empty() {
                return SaveOutcome::Invalid("start cannot be empty".to_string());
            }
            if dlg.end.trim().is_empty() {
                return SaveOutcome::Invalid("end cannot be empty".to_string());
            }
            let started_at = match timetrack_core::parse_moment(&dlg.start, "start", now, tz) {
                Ok(t) => t,
                Err(e) => return SaveOutcome::Invalid(e.to_string()),
            };
            let ended_at = match timetrack_core::parse_moment(&dlg.end, "end", now, tz) {
                Ok(t) => t,
                Err(e) => return SaveOutcome::Invalid(e.to_string()),
            };
            if ended_at < started_at {
                return SaveOutcome::Invalid(
                    timetrack_core::RuleError::NegativeDuration.to_string(),
                );
            }
            SaveOutcome::Create {
                project_id: dlg.project_id.clone(),
                description: dlg.description.clone(),
                started_at,
                ended_at,
            }
        }
        Some(id) => {
            let started_at = if dlg.start.trim().is_empty() {
                dlg.orig_start
            } else {
                match timetrack_core::parse_moment(&dlg.start, "start", now, tz) {
                    Ok(t) => t,
                    Err(e) => return SaveOutcome::Invalid(e.to_string()),
                }
            };
            let ended_at = if dlg.end.trim().is_empty() {
                dlg.orig_end
            } else {
                match timetrack_core::parse_moment(&dlg.end, "end", now, tz) {
                    Ok(t) => t,
                    Err(e) => return SaveOutcome::Invalid(e.to_string()),
                }
            };
            if ended_at < started_at {
                return SaveOutcome::Invalid(
                    timetrack_core::RuleError::NegativeDuration.to_string(),
                );
            }
            let description =
                (dlg.description != dlg.orig_description).then(|| dlg.description.clone());
            if started_at == dlg.orig_start && ended_at == dlg.orig_end && description.is_none() {
                return SaveOutcome::NoChange;
            }
            SaveOutcome::Update {
                id: id.clone(),
                started_at,
                ended_at,
                description,
            }
        }
    }
}

/// The newest quick-add, which is what undo removes.
///
/// A free function over the snapshot rather than a method on the view, so the
/// rule is testable without constructing a GPUI view (which needs a live
/// service connection). Entries arrive newest first, so the first match is the
/// newest.
fn newest_quick_add(snap: &Snapshot) -> Option<String> {
    snap.entries
        .iter()
        .find(|e| e.source == timetrack_proto::EntrySource::QuickAdd)
        .map(|e| e.id.clone())
}

/// The split prompt: one moment field for the selected entry.
///
/// Plain keystroke editing like the entry dialog (type to append, backspace
/// to delete, enter to split, escape to cancel), and likewise pure state
/// over strings so the whole interaction is unit-testable without a window.
#[derive(Debug, Clone)]
struct SplitPrompt {
    /// The entry being split.
    entry_id: String,
    /// Its interval, for the title and the range pre-check.
    started_at: i64,
    ended_at: i64,
    /// The typed split point (`-30`, `09:30`, `now` -- the shared spellings).
    at: String,
    error: Option<String>,
}

/// Resolve a split prompt to the split instant.
///
/// Pure over the prompt plus an explicit clock and zone, like `resolve_save`,
/// so parsing, the range pre-check and the empty-field refusal are all
/// testable without a view or a service. The service re-validates the range
/// on execute; this pre-check only phrases the refusal for the typed moment.
fn resolve_split(prompt: &SplitPrompt, now: i64, tz: &timetrack_core::Tz) -> Result<i64, String> {
    if prompt.at.trim().is_empty() {
        return Err("enter a split point, e.g. -30 or 09:30".to_string());
    }
    let at_ms =
        timetrack_core::parse_moment(&prompt.at, "at", now, tz).map_err(|e| e.to_string())?;
    if at_ms <= prompt.started_at || at_ms >= prompt.ended_at {
        return Err(format!(
            "split point is outside entry '{}'",
            prompt.entry_id
        ));
    }
    Ok(at_ms)
}

/// Entries `m` would merge: the selected entry plus every same-project entry
/// it overlaps or touches.
///
/// Free over the snapshot like `newest_quick_add`, for the same reason.
/// Touching counts: joining end-to-end entries is the lossless case §8
/// exists for, and stopping at "overlap only" would refuse exactly that.
fn merge_targets(snapshot: &Snapshot, selected: usize) -> Vec<String> {
    let Some(picked) = snapshot.entries.get(selected) else {
        return Vec::new();
    };
    let mut ids = vec![picked.id.clone()];
    for e in &snapshot.entries {
        if e.id != picked.id
            && e.project_id == picked.project_id
            && e.started_at <= picked.ended_at
            && e.ended_at >= picked.started_at
        {
            ids.push(e.id.clone());
        }
    }
    if ids.len() < 2 { Vec::new() } else { ids }
}

/// A pending merge confirmation: the ids to send, and the one-line summary
/// the banner shows (count, project, and how the total changes).
#[derive(Debug, Clone)]
struct MergeConfirm {
    ids: Vec<String>,
    summary: String,
}

/// Build the merge confirmation for the selection, if there is anything to
/// merge. The delta comes from the shared core preview so the GUI can never
/// word it differently from the CLI.
fn merge_offer(snapshot: &Snapshot, selected: usize) -> Option<MergeConfirm> {
    let ids = merge_targets(snapshot, selected);
    if ids.len() < 2 {
        return None;
    }
    let by_id: std::collections::HashMap<&str, &timetrack_proto::EntryView> = snapshot
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let intervals: Vec<(i64, i64)> = ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()))
        .map(|e| (e.started_at, e.ended_at))
        .collect();
    let delta = timetrack_core::format_merge_delta(timetrack_core::merge_shrink_ms(&intervals));
    let project = by_id
        .get(ids[0].as_str())
        .and_then(|e| {
            snapshot
                .projects
                .iter()
                .find(|p| p.id == e.project_id)
                .map(|p| p.name.clone())
        })
        .unwrap_or_default();
    Some(MergeConfirm {
        summary: format!(
            "MERGE {} ENTRIES on {project}? {delta} — m to confirm, esc to cancel",
            ids.len()
        ),
        ids,
    })
}

/// A pending delete confirmation: the id to send, and the one-line summary
/// the banner shows (entry, project, duration) so `d` can never remove time
/// as a side effect of reaching for another key.
#[derive(Debug, Clone)]
struct DeleteConfirm {
    id: String,
    summary: String,
}

/// Build the delete confirmation for the selection, if there is one.
///
/// Free over the snapshot like `merge_offer`, for the same reason. The
/// summary names the entry, its project and its duration so the confirm
/// banner states exactly what would be lost.
fn delete_offer(snapshot: &Snapshot, selected: usize) -> Option<DeleteConfirm> {
    let picked = snapshot.entries.get(selected)?.clone();
    let project = snapshot
        .projects
        .iter()
        .find(|p| p.id == picked.project_id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| picked.project_id.clone());
    let what = if picked.description.is_empty() {
        "no note".to_string()
    } else {
        format!("'{}'", picked.description)
    };
    Some(DeleteConfirm {
        summary: format!(
            "DELETE {} ({} · {} · {})? d to confirm, esc to cancel",
            picked.id,
            project,
            timetrack_core::format_duration(picked.duration_ms()),
            what,
        ),
        id: picked.id,
    })
}

/// The project prompt: one name field, for creating (`edit_id` none) or
/// renaming (`edit_id` set, prefilled with the current name).
///
/// Plain keystroke editing like the other prompts, and likewise pure state
/// so resolving is unit-testable without a window.
#[derive(Debug, Clone)]
struct ProjectPrompt {
    edit_id: Option<String>,
    /// The typed name. Prefilled when renaming, so saving untouched would
    /// be a no-op the service round-trips harmlessly.
    name: String,
    error: Option<String>,
}

/// What saving a project prompt means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectSave {
    Create { name: String },
    Update { id: String, name: String },
}

/// Resolve a project prompt. Empty stays open with an error rather than
/// storing a label no picker could show; duplicates travel to the service,
/// which owns name uniqueness and reports them typed.
fn resolve_project_prompt(prompt: &ProjectPrompt) -> Result<ProjectSave, String> {
    if prompt.name.trim().is_empty() {
        return Err("enter a project name".to_string());
    }
    let name = prompt.name.trim().to_string();
    match &prompt.edit_id {
        None => Ok(ProjectSave::Create { name }),
        Some(id) => Ok(ProjectSave::Update {
            id: id.clone(),
            name,
        }),
    }
}

/// Which prompt, if any, currently owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenPrompt {
    Entry,
    Project,
    Split,
    Merge,
    Delete,
}

/// Which prompt is open, if one is.
///
/// The key handler and the renderer ask this same question, so a prompt that
/// captures the keyboard is always one that is on screen.
///
/// They used to disagree. The key handler dispatched to the prompts on every
/// tab, while the renderer drew them inside `render_home` -- so opening one on
/// the Projects or Export tab produced a modal that was never drawn and that
/// swallowed every keystroke: the window looked frozen and only `Esc` got
/// out. One question, asked once, is what keeps the two in step.
///
/// At most one prompt is open at a time (each one swallows the keys that
/// would open another), so the order below is the priority the key handler
/// already used: the entry dialog first, then the project prompt, the split
/// prompt, the merge confirmation and the delete confirmation.
fn which_prompt(
    entry: bool,
    project: bool,
    split: bool,
    merge: bool,
    delete: bool,
) -> Option<OpenPrompt> {
    if entry {
        Some(OpenPrompt::Entry)
    } else if project {
        Some(OpenPrompt::Project)
    } else if split {
        Some(OpenPrompt::Split)
    } else if merge {
        Some(OpenPrompt::Merge)
    } else if delete {
        Some(OpenPrompt::Delete)
    } else {
        None
    }
}

/// The element id of one of a project's row buttons.
///
/// Built from the project id itself rather than from a number parsed out of
/// it. The core mints ids as `p1`, `p2`, ..., so parsing the numeric suffix
/// was unique in practice -- but any id that was not `p<digits>` fell back to
/// `0`, which would have given two such projects the same element id, and
/// gpui would have treated the second button as the first.
fn project_button_id(kind: &'static str, project_id: &str) -> ElementId {
    ElementId::from((ElementId::from(kind), project_id.to_string()))
}

/// The Export tab's scope-plus-anchor picker (spec #19).
///
/// One shared anchor selects which week or month the stepper and the typed
/// day point at; all time ignores it. The typed day accepts the terminal day
/// spelling (`YYYY-MM-DD`) parsed with the snapshot zone, exactly like the
/// CLI's `--at`. The preview shows the first rows of the scoped export
/// oldest first following this scope plus anchor, and the export buttons
/// below save that scope with this anchor (all time ignores it on both).
///
/// Plain state over values like the entry dialog, so every transition is
/// unit-testable without a window.
#[derive(Debug, Clone)]
struct ExportPicker {
    scope: timetrack_core::ExportScope,
    /// The instant selecting which week or month; ignored by all time.
    anchor_ms: i64,
    /// The typed day as shown in the field; mirrors the anchor's local day.
    anchor_text: String,
    /// Whether keystrokes go to the typed day field.
    editing: bool,
    /// The last bad day, shown inline; the anchor is unchanged.
    error: Option<String>,
}

impl ExportPicker {
    /// This week anchored at now: the common retrospective needs no typing.
    /// The typed day starts in UTC (the default snapshot's zone) and is
    /// re-mirrored from the snapshot zone on arrival via
    /// [`ExportPicker::refresh_from_snapshot`].
    fn new(now_ms: i64) -> Self {
        let utc = timetrack_core::Tz::utc();
        ExportPicker {
            scope: timetrack_core::ExportScope::Week,
            anchor_ms: now_ms,
            anchor_text: format_anchor_day(now_ms, &utc),
            editing: false,
            error: None,
        }
    }

    /// All time ignores the anchor, so its controls render inert.
    fn is_active(&self) -> bool {
        self.scope != timetrack_core::ExportScope::All
    }

    fn period_label(&self, tz: &timetrack_core::Tz) -> String {
        export_period_label(self.scope, self.anchor_ms, tz)
    }

    /// Switch week/month/all, keeping the shared anchor; the typed day
    /// re-mirrors it.
    fn set_scope(&mut self, scope: timetrack_core::ExportScope, tz: &timetrack_core::Tz) {
        self.scope = scope;
        self.editing = false;
        self.error = None;
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
    }

    /// One stepper press. Stepping leaves edit mode: the field mirrors the
    /// anchor again.
    fn step(&mut self, delta: i32, tz: &timetrack_core::Tz) {
        if !self.is_active() {
            return;
        }
        self.anchor_ms = shift_export_anchor(self.scope, self.anchor_ms, delta, tz);
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        self.editing = false;
        self.error = None;
    }

    /// Open the typed day field. Never for all time.
    fn start_editing(&mut self) {
        if !self.is_active() {
            return;
        }
        self.editing = true;
        self.error = None;
    }

    /// Close the field without committing, mirroring the anchor again.
    fn cancel_editing(&mut self, tz: &timetrack_core::Tz) {
        self.editing = false;
        self.error = None;
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
    }

    /// Append typed text to the day field, clearing a stale error.
    fn type_text(&mut self, ch: &str) {
        if !self.editing || !self.is_active() {
            return;
        }
        self.anchor_text.push_str(ch);
        self.error = None;
    }

    /// Delete the last character of the day field.
    fn backspace(&mut self) {
        if !self.editing || !self.is_active() {
            return;
        }
        self.anchor_text.pop();
        self.error = None;
    }

    /// Commit the typed day: the terminal spelling parsed with the snapshot
    /// zone. A bad day stays open with an inline error and the anchor is
    /// unchanged.
    fn commit(&mut self, tz: &timetrack_core::Tz) -> bool {
        if !self.editing || !self.is_active() {
            return false;
        }
        match timetrack_core::parse_anchor_day(&self.anchor_text, "anchor", tz) {
            Ok(anchor) => {
                self.anchor_ms = anchor;
                self.anchor_text = format_anchor_day(anchor, tz);
                self.error = None;
                self.editing = false;
                true
            }
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        }
    }

    /// Re-mirror the typed day from the snapshot zone after a snapshot
    /// arrives. Only the just-opened value is fixed: while the field is being
    /// typed (`editing`) the text is the user's, never the mirror.
    fn refresh_from_snapshot(&mut self, tz: &timetrack_core::Tz) {
        if !self.editing {
            self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        }
    }
}

/// The anchor's local day as `YYYY-MM-DD`, the spelling the typed field and
/// the CLI share. Thin wrapper over the shared core helper so the GUI mirror
/// and the shifters cannot disagree about the day boundary.
fn format_anchor_day(anchor_ms: i64, tz: &timetrack_core::Tz) -> String {
    timetrack_core::anchor_day_string(anchor_ms, tz)
}

/// What the stepper sits on: the ISO week id, the month id, or all time
/// (which has no id because its anchor is ignored). Thin wrapper over the
/// shared scope table so both UIs agree.
fn export_period_label(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    scope.period_id(anchor_ms, tz)
}

/// Move the anchor one scope step per `delta`. Thin wrapper over the shared
/// scope table: both week and month resolve through the day spelling, so a
/// DST edge cannot move the anchor off midday.
fn shift_export_anchor(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    delta: i32,
    tz: &timetrack_core::Tz,
) -> i64 {
    scope.shift(anchor_ms, delta, tz)
}

/// The three tabs (REQUIREMENTS §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    Projects,
    Export,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Projects => "Projects",
            Tab::Export => "Export",
        }
    }

    fn all() -> [Tab; 3] {
        [Tab::Home, Tab::Projects, Tab::Export]
    }
}

/// The tab after Tab / Shift+Tab from `current`, wrapping around both ends.
///
/// Free of the view so the cycle order is unit-testable: the key handler
/// only forwards the shift state.
fn cycle_tab(current: Tab, backward: bool) -> Tab {
    let tabs = Tab::all();
    let at = tabs.iter().position(|t| *t == current).unwrap_or(0);
    let next = if backward {
        (at + tabs.len() - 1) % tabs.len()
    } else {
        (at + 1) % tabs.len()
    };
    tabs[next]
}

/// Width of the per-project bar track on Home.
const PROJECT_BAR_TRACK_PX: f32 = 120.0;

/// Bar fill width for one per-project row: longest fills the track.
///
/// Pure arithmetic so the proportion is unit-testable; the renderer only
/// turns the number into pixels.
fn bar_width(ms: i64, max_ms: i64) -> f32 {
    if max_ms <= 0 {
        return 0.0;
    }
    (ms.max(0) as f32 / max_ms as f32) * PROJECT_BAR_TRACK_PX
}

pub struct TimetrackView {
    rx: Receiver<Msg>,
    actions: Sender<Action>,
    snapshot: Snapshot,
    status: Option<StatusMsg>,
    selected: usize,
    tab: Tab,
    /// Index into the active projects, for quick-add attribution.
    project_cursor: usize,
    /// Entity handle, used to attach click handlers from inside `render`
    /// (which only has `&self`).
    entity: Option<Entity<Self>>,
    /// Focus for the root element.
    ///
    /// This is not optional polish. gpui dispatches a key event along the
    /// ancestor path of the *focused* node only, so without a focused element
    /// the root's `on_key_down` never runs and the whole keyboard is dead --
    /// the window looks alive, tabs respond to clicks, and nothing else
    /// responds to anything. The handle is focused on the first frame.
    focus: FocusHandle,
    /// Whether the one-time focus-on-render has happened. Re-focusing every
    /// frame would steal focus back from anything focusable the user reaches
    /// later, which would break Tab navigation.
    focused_once: bool,
    /// The manual-entry dialog, when open. Modal: while this is `Some` every
    /// keystroke goes to the dialog and the app shortcuts are suspended.
    dialog: Option<EntryDialog>,
    /// The project prompt, when open. Modal like the dialog: `None` creates,
    /// `Some` with an id renames.
    project_prompt: Option<ProjectPrompt>,
    /// The split prompt, when open. Modal like the dialog.
    split_prompt: Option<SplitPrompt>,
    /// A pending merge confirmation. Only `m` (confirm), escape (cancel)
    /// and `q` (quit) do anything while this is `Some`.
    merge_confirm: Option<MergeConfirm>,
    /// A pending delete confirmation. Only `d` (confirm), escape (cancel)
    /// and `q` (quit) do anything while this is `Some`.
    delete_confirm: Option<DeleteConfirm>,
    /// The Export tab's scope-plus-anchor picker (spec #19). The anchor
    /// defaults to now at view creation; all time ignores it.
    export_picker: ExportPicker,
    /// Where the last export save went, and whether the portal granted it.
    /// `None` before the first save. Drives the Export tab's location line:
    /// a portal grant names its chosen path, anything else (direct save or
    /// no save yet) keeps the home-directory wording.
    export_last_dest: Option<String>,
    export_last_via_portal: bool,
}

/// The name prefilled in the system save dialog for a scope plus anchor
/// (ticket 27): the period-stamped default, so one week never silently
/// overwrites the last; all time ignores the anchor and keeps the plain name.
fn export_suggested_name(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    timetrack_core::default_export_filename(scope, anchor_ms, tz)
}

/// What the system save dialog answered (ticket 27).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PortalOutcome {
    /// The user confirmed: write through this granted path, never the
    /// direct fallback.
    Granted(String),
    /// The user cancelled: a quiet no-op status, writes nothing, never an
    /// error (ticket 28).
    Cancelled,
    /// No portal answered (headless CI): the direct home-directory save runs
    /// on as the automatic fallback.
    Unavailable,
}

/// Where the fetched CSV goes: the granted path on confirm, nowhere on
/// cancel (`None` means the caller sends the quiet cancel status and writes
/// nothing), the home-directory fallback when no portal is available.
fn resolve_export_destination(outcome: PortalOutcome, fallback: &str) -> Option<String> {
    match outcome {
        PortalOutcome::Granted(granted) => Some(granted),
        PortalOutcome::Cancelled => None,
        PortalOutcome::Unavailable => Some(fallback.to_string()),
    }
}

/// Headless-test seam for the save dialog (tickets 27-28).
///
/// When `TIMETRACK_PORTAL_STUB` is `record:<file>`, the requested dialog
/// name is appended to `<file>` and the portal counts as unavailable, so
/// headless runs exercise the dialog wiring (which name is requested) while
/// the home-directory fallback keeps working. Anything else is not a stub.
///
/// Edge-mode switches for the ticket-28 pins, still with no live portal:
/// touching `<file>.cancel` makes the next stubbed save answer Cancelled
/// (quiet no-op, writes nothing), and `<file>.fail` grants a path whose
/// parent does not exist (local error naming path plus reason). Both still
/// log the requested name first, so the dialog wiring stays pinned. Only
/// when the stub is active, so production never sees this.
fn portal_stub_outcome(stub: &str, current_name: &str) -> Option<PortalOutcome> {
    let log = stub.strip_prefix("record:")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok()?;
    use std::io::Write;
    writeln!(file, "{current_name}").ok()?;
    if std::path::Path::new(&format!("{log}.cancel")).exists() {
        return Some(PortalOutcome::Cancelled);
    }
    if std::path::Path::new(&format!("{log}.fail")).exists() {
        return Some(PortalOutcome::Granted(format!(
            "/nonexistent-timetrack-gui/{current_name}"
        )));
    }
    Some(PortalOutcome::Unavailable)
}

/// Ask the system save dialog where to write an export (ticket 27).
///
/// The period-stamped `current_name` is prefilled so one week never silently
/// overwrites the last; the portal's own overwrite question governs existing
/// files, so there is exactly one prompt. Cancel is quiet (`Cancelled`);
/// anything but a confirm or a cancel — no portal frontend, no session bus
/// — counts as unavailable and the direct fallback runs.
async fn portal_save_file(current_name: &str) -> PortalOutcome {
    if let Ok(stub) = std::env::var("TIMETRACK_PORTAL_STUB") {
        if let Some(outcome) = portal_stub_outcome(&stub, current_name) {
            return outcome;
        }
    }
    let request = ashpd::desktop::file_chooser::SelectedFiles::save_file()
        .title("Save export")
        .current_name(current_name)
        .send()
        .await;
    let files = match request {
        Ok(request) => match request.response() {
            Ok(files) => files,
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                return PortalOutcome::Cancelled;
            }
            Err(_) => return PortalOutcome::Unavailable,
        },
        Err(_) => return PortalOutcome::Unavailable,
    };
    let Some(uri) = files.uris().first() else {
        return PortalOutcome::Cancelled;
    };
    match uri.to_file_path() {
        Ok(path) => PortalOutcome::Granted(path.to_string_lossy().into_owned()),
        Err(_) => PortalOutcome::Unavailable,
    }
}

/// Quiet status after cancelling the system save dialog (ticket 28).
///
/// A no-op, never an error: changing your mind is not a failure, so this
/// travels as `Msg::Status`, writes nothing, and names no path.
fn export_cancelled_status() -> &'static str {
    "export cancelled"
}

/// The cancel outcome as one message: a quiet status, never error-styled.
fn export_cancel_msg() -> Msg {
    Msg::Status(export_cancelled_status().to_string())
}

/// Write fetched CSV bytes to `dest` and report the outcome as one message.
///
/// The granted portal path and the direct fallback share this: success
/// names count, scope, week or month id and file base name in the shared
/// core wording, while a failed write stays a local note with path and
/// reason, never a service error. An existing destination is overwritten:
/// the portal's own overwrite question already governed it, so there is
/// exactly one prompt, never a second app confirm. The scope travels typed
/// (nothing re-parses it); `via_portal` records whether the portal granted
/// the path so the Export tab can label a chosen path differently from the
/// direct fallback.
fn export_write_msg(
    csv: &str,
    dest: &str,
    scope: timetrack_core::ExportScope,
    period_id: &str,
    via_portal: bool,
) -> Msg {
    match std::fs::write(dest, csv) {
        Ok(()) => {
            let count = csv.lines().count().saturating_sub(1);
            let basename = dest.rsplit('/').next().unwrap_or(dest);
            let status = export_saved_status(count, scope, period_id, basename);
            Msg::ExportSaved {
                status,
                dest: dest.to_string(),
                via_portal,
            }
        }
        Err(e) => Msg::Error(StatusMsg {
            text: format!("could not write {dest}: {e}"),
            kind: StatusKind::Local,
        }),
    }
}

/// Where a GUI export lands while the portal is unreachable (ticket 27
/// fallback).
///
/// The app runs unsandboxed, so `$HOME` is the real home directory.
/// The name carries the week or month id so one week never silently
/// overwrites the last; all time ignores the anchor and keeps the plain
/// name.
fn default_export_path(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let name = export_suggested_name(scope, anchor_ms, tz);
    format!("{home}/{name}")
}

/// The label for the export buttons: without a portal the app writes
/// directly to the home directory, and the system save dialog (ticket 27)
/// is what offers any other location.
fn export_location_label() -> &'static str {
    "Direct save: writes to ~/"
}

/// The Export tab's location line given the last save: a portal grant wrote
/// to the chosen path, so it says so with the path; anything else (direct
/// save or no save yet) keeps the home-directory wording.
fn export_location_text(last: Option<(&str, bool)>) -> String {
    match last {
        Some((dest, true)) => format!("Saved via system dialog: {dest}"),
        _ => export_location_label().to_string(),
    }
}

/// Status line after an interim save: entry count, scope, week or month id,
/// and file base name, in the shared core wording.
fn export_saved_status(
    entry_count: usize,
    scope: timetrack_core::ExportScope,
    period_id: &str,
    basename: &str,
) -> String {
    timetrack_core::format_export_status(entry_count, scope, period_id, basename)
}

/// Which service call an interim save uses: `None` is the old scope-only
/// call (anchored at now by the service), `Some(anchor)` the scope-plus-
/// anchor call. All time always takes the old call however stale the anchor
/// is, and a current week or month does too, so old services keep serving
/// the common case; only a chosen week or month needs the new call.
fn export_anchor_for_save(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    now_ms: i64,
    tz: &timetrack_core::Tz,
) -> Option<i64> {
    match scope {
        timetrack_core::ExportScope::All => None,
        _ if scope.period_id(anchor_ms, tz) == scope.period_id(now_ms, tz) => None,
        _ => Some(anchor_ms),
    }
}

/// The rows the scoped export will hold, oldest first, over the snapshot's
/// entries with the snapshot zone (spec #19).
///
/// Filters with the shared core predicate ([`timetrack_core::entry_in_scope`]:
/// `started_at` wins per ADR-0002, all time ignores the anchor), so the
/// preview shows what the file will hold without a second service call and
/// cannot disagree with the service bucketing. The snapshot arrives newest
/// first; the preview re-sorts oldest first, the order a stakeholder reads.
fn export_preview_entries<'a>(
    snapshot: &'a Snapshot,
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> Vec<&'a timetrack_proto::EntryView> {
    let mut out: Vec<&timetrack_proto::EntryView> = snapshot
        .entries
        .iter()
        .filter(|e| timetrack_core::entry_in_scope(e.started_at, scope, anchor_ms, tz))
        .collect();
    out.sort_by_key(|e| e.started_at);
    out
}

impl TimetrackView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (rx, actions) = spawn_service_thread();
        let view = TimetrackView {
            rx,
            actions,
            snapshot: Snapshot::default(),
            status: None,
            selected: 0,
            tab: Tab::Home,
            project_cursor: 0,
            entity: None,
            focus: cx.focus_handle(),
            focused_once: false,
            dialog: None,
            project_prompt: None,
            split_prompt: None,
            merge_confirm: None,
            delete_confirm: None,
            export_picker: ExportPicker::new(now_ms()),
            export_last_dest: None,
            export_last_via_portal: false,
        };
        view.schedule_poll(cx);
        view
    }

    /// Re-render a few times a second so a queued service snapshot shows up
    /// promptly.
    ///
    /// `cx.spawn` hands the future a `WeakEntity`, so the entity is not kept
    /// alive by its own timer task; if the window closes the upgrade fails and
    /// the loop ends.
    fn schedule_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                // `update` alone does not re-render: gpui only paints after
                // `notify`, so a drained snapshot with no notify would sit in
                // state until the next pointer motion happened to repaint.
                if this
                    .update(cx, |view, cx| {
                        if view.drain() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    // NOTE: the service connection deliberately does NOT happen inside a
    // `cx.spawn` future. gpui drives those on its own executor threads, and
    // zbus' blocking/async-io backend is fine there but its tokio backend is
    // not. Everything bus-related is confined to the dedicated std::thread in
    // `spawn_service_thread`, which owns an async-io runtime of its own.

    /// Fold every queued message into one refresh. Several may be pending and
    /// only the newest snapshot matters.
    ///
    /// Returns whether anything visible changed, so the background poll can
    /// skip the re-render when the service had nothing new to say instead of
    /// repainting several times a second forever.
    fn drain(&mut self) -> bool {
        let mut changed = false;
        // Set the status only when it differs: the service thread resends
        // nothing, but snapshots arrive every second and must not count as
        // change on their own.
        let set_status = |status: &mut Option<StatusMsg>, note: StatusMsg, changed: &mut bool| {
            if status.as_ref() != Some(&note) {
                *status = Some(note);
                *changed = true;
            }
        };
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Snapshot(s)) => {
                    if *s != self.snapshot {
                        self.snapshot = *s;
                        changed = true;
                    }
                    // The typed day mirrors the anchor's local day: re-mirror
                    // it from the snapshot zone now that a real zone exists.
                    // User edits stay intact — only the just-opened value is
                    // fixed (see `refresh_from_snapshot`).
                    let tz = snapshot_tz(&self.snapshot);
                    let before = self.export_picker.anchor_text.clone();
                    self.export_picker.refresh_from_snapshot(&tz);
                    if self.export_picker.anchor_text != before {
                        changed = true;
                    }
                    // A routine snapshot must not wipe feedback from the last
                    // action: snapshots arrive every second, so clearing here
                    // made error reports from the service thread unreadable.
                    // Only the "service is not running" notice clears on
                    // recovery; action errors survive until the next one.
                    if self.service_missing() {
                        self.status = None;
                        changed = true;
                    }
                    // The lists can shrink under the cursor when something is
                    // removed, which would otherwise index out of bounds.
                    let entries = self.snapshot.entries.len();
                    if self.selected >= entries {
                        let clamped = entries.saturating_sub(1);
                        if clamped != self.selected {
                            self.selected = clamped;
                            changed = true;
                        }
                    }
                    let projects = self.active_projects().len();
                    if self.project_cursor >= projects {
                        let clamped = projects.saturating_sub(1);
                        if clamped != self.project_cursor {
                            self.project_cursor = clamped;
                            changed = true;
                        }
                    }
                }
                Ok(Msg::Error(note)) => set_status(&mut self.status, note, &mut changed),
                Ok(Msg::Status(note)) => {
                    set_status(&mut self.status, StatusMsg::local(note), &mut changed)
                }
                Ok(Msg::ExportSaved {
                    status,
                    dest,
                    via_portal,
                }) => {
                    set_status(&mut self.status, StatusMsg::local(status), &mut changed);
                    if self.export_last_dest.as_deref() != Some(dest.as_str())
                        || self.export_last_via_portal != via_portal
                    {
                        self.export_last_dest = Some(dest);
                        self.export_last_via_portal = via_portal;
                        changed = true;
                    }
                }
                Ok(Msg::ActionOk) => {
                    // A success dismisses the previous error -- but only one
                    // the actions own. The "service is not running" notice
                    // belongs to the connection, not to any action.
                    if !self.service_missing() && self.status.is_some() {
                        self.status = None;
                        changed = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    set_status(
                        &mut self.status,
                        StatusMsg::local("the service thread stopped"),
                        &mut changed,
                    );
                    break;
                }
            }
        }
        changed
    }

    fn send(&self, a: Action) {
        let _ = self.actions.send(a);
    }

    /// Projects offered for new entries: archived ones are hidden, because
    /// archiving is about not choosing them, not about erasing their history
    /// (REQUIREMENTS §14).
    fn active_projects(&self) -> Vec<&ProjectView> {
        self.snapshot
            .projects
            .iter()
            .filter(|p| !p.archived)
            .collect()
    }

    fn current_project(&self) -> Option<&ProjectView> {
        self.active_projects().get(self.project_cursor).copied()
    }

    /// Which prompt is open, if one is.
    ///
    /// The key handler and the renderer both ask this, so a prompt that
    /// captures the keyboard is always one that is drawn -- on whichever tab
    /// is showing.
    fn open_prompt(&self) -> Option<OpenPrompt> {
        which_prompt(
            self.dialog.is_some(),
            self.project_prompt.is_some(),
            self.split_prompt.is_some(),
            self.merge_confirm.is_some(),
            self.delete_confirm.is_some(),
        )
    }

    /// Handle a key press. Returns false when the app should quit.
    fn on_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        // A prompt that is open owns the keyboard, including the letters the
        // app shortcuts are bound to. Which prompt is open is one question
        // (`open_prompt`), and the renderer asks the same one -- so a prompt
        // that takes the keyboard is always one that is on screen.
        match self.open_prompt() {
            Some(OpenPrompt::Entry) => return self.on_dialog_key(ev),
            Some(OpenPrompt::Project) => return self.on_project_key(ev),
            Some(OpenPrompt::Split) => return self.on_split_key(ev),
            // A merge or a delete waits its turn: the anchor field is typed
            // into first, and each is answered by its own key handler below.
            _ => {}
        }
        // The anchor day field owns the keyboard while it is being typed
        // in, like the modal prompts do.
        if self.export_picker.editing {
            return self.on_anchor_key(ev);
        }
        // A pending merge or delete only listens for its own answer.
        match self.open_prompt() {
            Some(OpenPrompt::Merge) => return self.on_merge_key(ev),
            Some(OpenPrompt::Delete) => return self.on_delete_key(ev),
            _ => {}
        }
        // Tab cycles the three tabs (§11); shift goes backward. The Tab key
        // carries no printable char, so this keys on the key name, like the
        // dialog's field cycling does.
        if ev.keystroke.key.as_str() == "tab" {
            self.tab = cycle_tab(self.tab, ev.keystroke.modifiers.shift);
            return true;
        }
        let Some(ch) = ev.keystroke.key_char.as_deref() else {
            return true;
        };
        match ch {
            "q" => return false,

            // 1-4 map to the quick-add buckets (REQUIREMENTS §6).
            "1" => self.quick_add(QUICK_ADD_MS[0]),
            "2" => self.quick_add(QUICK_ADD_MS[1]),
            "3" => self.quick_add(QUICK_ADD_MS[2]),
            "4" => self.quick_add(QUICK_ADD_MS[3]),

            // The manual-entry dialog (REQUIREMENTS §5): `a` for a new
            // interval, `e` to adjust the selected entry.
            "a" => self.open_new_dialog(),
            "e" => self.open_edit_dialog(),

            // Projects (§11): `n` names a new one, `r` renames the current
            // one. No numbered placeholders anywhere: every name is typed.
            "n" => self.open_project_prompt(),
            "r" => self.open_rename_prompt(),

            // Undo removes only the quick-added entry; delete is explicit.
            "u" => match newest_quick_add(&self.snapshot) {
                Some(id) => self.send(Action::UndoQuickAdd(id)),
                None => self.status = Some(StatusMsg::local("nothing to undo")),
            },
            "d" => self.open_delete_confirm(),

            // Split and merge (REQUIREMENTS §8).
            "s" => self.open_split_prompt(),
            "m" => self.open_merge_confirm(),

            // Export anchor picker (spec #19): one press moves one period,
            // and `t` opens the typed day. Only on the Export tab; elsewhere
            // these keys stay unbound.
            "[" => {
                if self.tab == Tab::Export {
                    self.step_export_anchor(-1);
                }
            }
            "]" => {
                if self.tab == Tab::Export {
                    self.step_export_anchor(1);
                }
            }
            "t" => {
                if self.tab == Tab::Export {
                    self.export_picker.start_editing();
                }
            }

            "h" => self.project_cursor = self.project_cursor.saturating_sub(1),
            "l" => {
                if self.project_cursor + 1 < self.active_projects().len() {
                    self.project_cursor += 1;
                }
            }
            "j" => {
                if self.selected + 1 < self.snapshot.entries.len() {
                    self.selected += 1;
                }
            }
            "k" => self.selected = self.selected.saturating_sub(1),

            _ => {}
        }
        true
    }

    /// Handle a key press while the entry dialog is open. Always returns true:
    /// `q` types a "q" here rather than quitting, and `escape` cancels.
    fn on_dialog_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.dialog = None;
            }
            "enter" => self.dialog_save(),
            "tab" => {
                if let Some(dlg) = self.dialog.as_mut() {
                    if ev.keystroke.modifiers.shift {
                        dlg.field = dlg.field.prev();
                    } else {
                        dlg.field = dlg.field.next();
                    }
                }
            }
            "backspace" => {
                if let Some(dlg) = self.dialog.as_mut() {
                    dlg.backspace();
                }
            }
            _ => {
                // Printable text, and only that: with control, alt or the
                // platform modifier held the keystroke is a shortcut, not
                // input, and key_char of a special key must not leak in.
                let m = &ev.keystroke.modifiers;
                if !m.control && !m.alt && !m.platform {
                    if let Some(ch) = ev.keystroke.key_char.as_deref() {
                        if let Some(dlg) = self.dialog.as_mut() {
                            dlg.type_text(ch);
                        }
                    }
                }
            }
        }
        true
    }

    /// Open a new-entry dialog for the current project.
    fn open_new_dialog(&mut self) {
        let current = self
            .current_project()
            .map(|p| (p.id.clone(), p.name.clone()));
        match current {
            Some((id, name)) => {
                self.status = None;
                self.dialog = Some(EntryDialog::new(id, name));
            }
            None => {
                self.status = Some(StatusMsg::local(
                    "create a project on the Projects tab before adding time",
                ));
            }
        }
    }

    /// Open an edit dialog for the selected entry: new endpoints, or new
    /// text, or both. Empty times keep the current endpoints.
    fn open_edit_dialog(&mut self) {
        match self.snapshot.entries.get(self.selected).cloned() {
            Some(e) => {
                let project = self
                    .snapshot
                    .projects
                    .iter()
                    .find(|p| p.id == e.project_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| e.project_id.clone());
                self.status = None;
                self.dialog = Some(EntryDialog::edit(
                    e.id,
                    project,
                    e.description,
                    e.started_at,
                    e.ended_at,
                ));
            }
            None => self.status = Some(StatusMsg::local("nothing to edit")),
        }
    }

    /// Open the project prompt for a new, typed name.
    fn open_project_prompt(&mut self) {
        self.status = None;
        self.project_prompt = Some(ProjectPrompt {
            edit_id: None,
            name: String::new(),
            error: None,
        });
    }

    /// Open the project prompt to rename the current project, prefilled so
    /// small fixes keep most of the name.
    fn open_rename_prompt(&mut self) {
        let current = self
            .current_project()
            .map(|p| (p.id.clone(), p.name.clone()));
        match current {
            Some((id, name)) => {
                self.status = None;
                self.project_prompt = Some(ProjectPrompt {
                    edit_id: Some(id),
                    name,
                    error: None,
                });
            }
            None => self.status = Some(StatusMsg::local("nothing to rename")),
        }
    }

    /// Handle a key press while the project prompt is open. Always returns
    /// true: `q` types a "q" here rather than quitting, and `escape` cancels.
    fn on_project_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.project_prompt = None;
            }
            "enter" => self.project_save(),
            "backspace" => {
                if let Some(prompt) = self.project_prompt.as_mut() {
                    prompt.name.pop();
                    prompt.error = None;
                }
            }
            _ => {
                // Printable text, and only that: same gate as the other
                // prompts, so shortcuts never leak into the name.
                let m = &ev.keystroke.modifiers;
                if !m.control && !m.alt && !m.platform {
                    if let Some(ch) = ev.keystroke.key_char.as_deref() {
                        if let Some(prompt) = self.project_prompt.as_mut() {
                            prompt.name.push_str(ch);
                            prompt.error = None;
                        }
                    }
                }
            }
        }
        true
    }

    /// Validate the project prompt and send it. Stays open with an error on
    /// empty input; closes on success. Duplicate names travel to the service,
    /// which owns uniqueness and reports them typed.
    fn project_save(&mut self) {
        let Some(prompt) = self.project_prompt.clone() else {
            return;
        };
        match resolve_project_prompt(&prompt) {
            Err(msg) => {
                if let Some(open) = self.project_prompt.as_mut() {
                    open.error = Some(msg);
                }
            }
            Ok(ProjectSave::Create { name }) => {
                self.send(Action::AddProject(name));
                self.project_prompt = None;
            }
            Ok(ProjectSave::Update { id, name }) => {
                self.send(Action::UpdateProject { id, name });
                self.project_prompt = None;
            }
        }
    }

    /// Open the split prompt for the selected entry, if there is one.
    fn open_split_prompt(&mut self) {
        match self.snapshot.entries.get(self.selected).cloned() {
            Some(e) => {
                self.status = None;
                self.split_prompt = Some(SplitPrompt {
                    entry_id: e.id,
                    started_at: e.started_at,
                    ended_at: e.ended_at,
                    at: String::new(),
                    error: None,
                });
            }
            None => self.status = Some(StatusMsg::local("nothing to split")),
        }
    }

    /// Handle a key press while the split prompt is open. Always returns
    /// true: `q` types a "q" here rather than quitting, and `escape` cancels.
    fn on_split_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.split_prompt = None;
            }
            "enter" => self.split_save(),
            "backspace" => {
                if let Some(prompt) = self.split_prompt.as_mut() {
                    prompt.at.pop();
                    prompt.error = None;
                }
            }
            _ => {
                // Printable text, and only that: same gate as the dialog, so
                // shortcuts and special keys never leak into the moment.
                let m = &ev.keystroke.modifiers;
                if !m.control && !m.alt && !m.platform {
                    if let Some(ch) = ev.keystroke.key_char.as_deref() {
                        if let Some(prompt) = self.split_prompt.as_mut() {
                            prompt.at.push_str(ch);
                            prompt.error = None;
                        }
                    }
                }
            }
        }
        true
    }

    /// Validate the split prompt and send it. Stays open with an error on
    /// invalid input; closes on success.
    fn split_save(&mut self) {
        let Some(prompt) = self.split_prompt.clone() else {
            return;
        };
        let tz = snapshot_tz(&self.snapshot);
        match resolve_split(&prompt, now_ms(), &tz) {
            Err(msg) => {
                if let Some(open) = self.split_prompt.as_mut() {
                    open.error = Some(msg);
                }
            }
            Ok(at_ms) => {
                self.send(Action::SplitEntry {
                    id: prompt.entry_id,
                    at_ms,
                });
                self.split_prompt = None;
            }
        }
    }

    /// Offer the merge confirmation for the selection, if the selected entry
    /// overlaps or touches anything on its project.
    fn open_merge_confirm(&mut self) {
        match merge_offer(&self.snapshot, self.selected) {
            Some(confirm) => {
                self.status = None;
                self.merge_confirm = Some(confirm);
            }
            None => {
                self.status = Some(StatusMsg::local("nothing to merge with it"));
            }
        }
    }

    /// Handle a key press while a merge confirmation is pending. Only the
    /// confirmation's own answers do anything; anything else is swallowed so
    /// no other action can sneak in mid-confirmation.
    fn on_merge_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        if ev.keystroke.key.as_str() == "escape" {
            self.merge_confirm = None;
            return true;
        }
        match ev.keystroke.key_char.as_deref() {
            Some("m") => {
                if let Some(confirm) = self.merge_confirm.take() {
                    self.send(Action::MergeEntries { ids: confirm.ids });
                }
                true
            }
            Some("q") => false,
            _ => true,
        }
    }

    /// Offer the delete confirmation for the selection, if there is one.
    /// The first `d` only opens the banner; a second `d` executes it.
    fn open_delete_confirm(&mut self) {
        match delete_offer(&self.snapshot, self.selected) {
            Some(confirm) => {
                self.status = None;
                self.delete_confirm = Some(confirm);
            }
            None => {
                self.status = Some(StatusMsg::local("nothing to delete"));
            }
        }
    }

    /// Handle a key press while a delete confirmation is pending. Only the
    /// confirmation's own answers do anything; anything else is swallowed so
    /// no other action can sneak in mid-confirmation.
    fn on_delete_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        if ev.keystroke.key.as_str() == "escape" {
            self.delete_confirm = None;
            return true;
        }
        match ev.keystroke.key_char.as_deref() {
            Some("d") => {
                if let Some(confirm) = self.delete_confirm.take() {
                    self.send(Action::DeleteEntry(confirm.id));
                }
                true
            }
            Some("q") => false,
            _ => true,
        }
    }

    /// Validate the dialog and send the action(s). Stays open with an error
    /// on invalid input; closes on success or when nothing changed.
    fn dialog_save(&mut self) {
        let Some(dlg) = self.dialog.as_ref() else {
            return;
        };
        let (orig_start, orig_end) = (dlg.orig_start, dlg.orig_end);
        let tz = snapshot_tz(&self.snapshot);
        match resolve_save(dlg, now_ms(), &tz) {
            SaveOutcome::Invalid(msg) => {
                if let Some(dlg) = self.dialog.as_mut() {
                    dlg.error = Some(msg);
                }
            }
            SaveOutcome::NoChange => {
                self.dialog = None;
            }
            SaveOutcome::Create {
                project_id,
                description,
                started_at,
                ended_at,
            } => {
                self.send(Action::AddEntry {
                    project: project_id,
                    description,
                    started_at,
                    ended_at,
                });
                self.dialog = None;
            }
            SaveOutcome::Update {
                id,
                started_at,
                ended_at,
                description,
            } => {
                // Only call for what actually changed: an identical SetTimes
                // would persist and acknowledge a no-op write.
                if started_at != orig_start || ended_at != orig_end {
                    self.send(Action::SetTimes {
                        id: id.clone(),
                        started_at,
                        ended_at,
                    });
                }
                if let Some(description) = description {
                    self.send(Action::SetText { id, description });
                }
                self.dialog = None;
            }
        }
    }

    /// Move the export anchor one period in `delta` (±1 per press).
    fn step_export_anchor(&mut self, delta: i32) {
        let tz = snapshot_tz(&self.snapshot);
        self.export_picker.step(delta, &tz);
    }

    /// The Export tab's location line: a portal grant names its chosen path,
    /// anything else keeps the home-directory wording.
    fn export_location_text(&self) -> String {
        export_location_text(
            self.export_last_dest
                .as_deref()
                .map(|dest| (dest, self.export_last_via_portal)),
        )
    }

    /// Handle a key press while the anchor day field is open. Always returns
    /// true: `q` types a "q" here rather than quitting, and `escape` cancels.
    fn on_anchor_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        match ev.keystroke.key.as_str() {
            "escape" => {
                let tz = snapshot_tz(&self.snapshot);
                self.export_picker.cancel_editing(&tz);
            }
            "enter" => {
                let tz = snapshot_tz(&self.snapshot);
                self.export_picker.commit(&tz);
            }
            "backspace" => self.export_picker.backspace(),
            // Tab stays in the field: enter sets the day, escape cancels.
            "tab" => {}
            _ => {
                // Printable text, and only that: same gate as the dialogs, so
                // shortcuts and special keys never leak into the day.
                let m = &ev.keystroke.modifiers;
                if !m.control && !m.alt && !m.platform {
                    if let Some(ch) = ev.keystroke.key_char.as_deref() {
                        self.export_picker.type_text(ch);
                    }
                }
            }
        }
        true
    }

    /// Add one of the quick-add buckets to the selected project.
    fn quick_add(&mut self, duration_ms: i64) {
        match self.current_project() {
            Some(p) => {
                let project = p.id.clone();
                self.send(Action::QuickAdd {
                    project,
                    ms: duration_ms,
                });
            }
            None => {
                self.status = Some(StatusMsg::local(
                    "create a project on the Projects tab before adding time",
                ));
            }
        }
    }

    /// Whether the service has not been seen yet, in which case the window
    /// offers to start it instead of just complaining. Keys on the status
    /// kind, never on its wording.
    fn service_missing(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.kind == StatusKind::NoService)
    }

    fn try_start_service(&self) {
        self.send(Action::StartService);
    }

    fn footer(&self) -> String {
        self.status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_else(|| {
            if self.dialog.is_some() {
                "type to edit · tab next field · enter save · esc cancel".to_string()
            } else if self.split_prompt.is_some() {
                "type a moment · enter split · esc cancel".to_string()
            } else if self.merge_confirm.is_some() {
                "m to confirm the merge · esc to cancel".to_string()
            } else if self.delete_confirm.is_some() {
                "d to confirm the delete · esc to cancel".to_string()
            } else if self.project_prompt.is_some() {
                "type a name · enter save · esc cancel".to_string()
            } else if self.export_picker.editing {
                "type a day YYYY-MM-DD · enter set · esc cancel".to_string()
            } else {
                "1-4 quick add · a add · e edit · s split · m merge · n project · r rename · u undo · d delete · tab tabs · j/k move · h/l project · q quit"
                    .to_string()
            }
        })
    }
}

impl Render for TimetrackView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drain here as well as in the poll loop: a frame already being built
        // for an event should pick up any snapshot waiting in the channel
        // rather than painting one frame stale.
        self.drain();

        // Focus the root on the first frame. `FocusHandle::focus` needs a
        // `&mut Window`, which `new` does not have, and doing it here is
        // equivalent: gpui cannot dispatch a key event until a frame exists to
        // dispatch it against, and the first render is that frame. Without
        // this the keyboard is dead until the user clicks something focusable.
        if !self.focused_once {
            self.focused_once = true;
            self.focus.focus(window);
        }

        let this = cx.entity();
        self.entity = Some(this.clone());
        let maximized = window.is_maximized();

        // The titlebar is the only chrome above the content, and it is chrome
        // rather than data -- the week total is still the first *number* on
        // screen and nothing of substance sits above it.
        //
        // The `.id()` and `.track_focus()` are load-bearing, not decoration:
        // gpui dispatches key events along the ancestor path of the focused
        // node, and an element with no id is not in the dispatch tree at all.
        //
        // The window itself is transparent (see `main.rs`). When floating,
        // the panel below rounds all four corners; the titlebar rounds
        // its own top because the panel clip does not round children. A
        // maximized window stays square and full-bleed: rounded corners
        // would leave gaps at the screen edges.
        div()
            .id("root")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .text_color(rgb(0xe6e6f0))
            .on_key_down(move |ev, _window, cx| {
                // `update` alone never repaints: it only runs the closure,
                // and a frame is scheduled solely by `notify`. Without it a
                // keystroke changes state that no frame ever shows.
                this.update(cx, |view, cx| {
                    if !view.on_key(ev) {
                        cx.quit();
                    } else {
                        cx.notify();
                    }
                });
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .bg(rgb(0x11111b))
                    .when(!maximized, |el| el.rounded_xl().overflow_hidden())
                    .child(self.render_titlebar(window))
                    // The prompt panels sit above the tab content, so a prompt
                    // opened on any tab is on screen. They are drawn here
                    // rather than inside the tab because the shortcuts that
                    // open them are global.
                    .child(self.render_prompts())
                    .child(match self.tab {
                        Tab::Home => self.render_home().into_any_element(),
                        Tab::Projects => self.render_projects().into_any_element(),
                        Tab::Export => self.render_export().into_any_element(),
                    })
                    .child(self.render_footer()),
            )
    }
}

/// A region of the integrated titlebar that drags the undecorated window.
///
/// Left-drag moves the window, right-click opens the window menu, and a
/// double-click toggles maximize, like a system title bar. The platform
/// ignores `window_control_area` on Linux, so the move starts here
/// explicitly; where the marker is honoured it does the same job.
fn titlebar_drag(id: &'static str, stretch: bool, label: &str) -> impl IntoElement {
    let mut region = div()
        .id(ElementId::from(id))
        .px_2()
        .py_1()
        .text_sm()
        .text_color(rgb(0x7a7a90))
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |_, window, _| {
            window.start_window_move();
        })
        .on_mouse_down(MouseButton::Right, |ev, window, _| {
            window.show_window_menu(ev.position);
        })
        .on_click(|ev, window, _| {
            if ev.click_count() == 2 {
                window.zoom_window();
            }
        });
    if stretch {
        region = region.flex_1();
    }
    region.child(label.to_string())
}

/// The maximize button's glyph: a square to expand, nested squares to
/// restore, following the window's current state.
fn maximize_glyph(is_maximized: bool) -> &'static str {
    if is_maximized { "▣" } else { "□" }
}

/// One of the integrated window controls: minimize, maximize or close.
///
/// Neutral grey, never the action green (which the smoke check counts), with
/// the close glyph in red. The `window_control_area` marker is honoured on
/// platforms with native hit-testing; the click handler below is what acts on
/// Linux, where the marker is ignored.
fn window_button(
    id: &'static str,
    label: &str,
    area: WindowControlArea,
    danger: bool,
    on_click: impl Fn(&mut Window) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::from(id))
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(if danger { rgb(0xe06c75) } else { rgb(0x9a9ab0) })
        .child(label.to_string())
        .window_control_area(area)
        .on_click(move |_, window, _| on_click(window))
}

impl TimetrackView {
    /// The integrated titlebar: the window has no system title bar (see
    /// `main.rs`), so this row is the drag handle, the tab strip and the
    /// minimize/maximize/close buttons in one.
    ///
    /// Only the title and the spacer drag. The tabs and buttons are siblings
    /// of those regions, never children, so pressing one can never start a
    /// move as a side effect of the event bubbling up to a draggable parent.
    fn render_titlebar(&self, window: &Window) -> impl IntoElement {
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgb(0x1a1a26))
            // The panel clip does not round children, so the titlebar
            // rounds its own top to meet the panel's corners. Square when
            // maximized, like the panel itself.
            .when(!window.is_maximized(), |el| el.rounded_t_xl());
        row = row.child(titlebar_drag("titlebar-drag", false, "TimeTrack"));
        for tab in Tab::all() {
            let is_selected = tab == self.tab;
            row = row.child(
                div()
                    .id(ElementId::from(tab.label()))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if is_selected {
                        rgba(0x2a2a3aff)
                    } else {
                        rgba(0x00000000)
                    })
                    .text_color(if is_selected {
                        rgb(0xffffff)
                    } else {
                        rgb(0x7a7a90)
                    })
                    .child(tab.label())
                    .when(!is_selected, move |el| {
                        let entity = self.entity.clone();
                        el.on_click(move |_ev, _window, cx| {
                            if let Some(entity) = entity.clone() {
                                entity.update(cx, |view, cx| {
                                    view.tab = tab;
                                    cx.notify();
                                });
                            }
                        })
                    }),
            );
        }
        row = row.child(titlebar_drag("titlebar-spacer", true, ""));
        row = row
            .child(window_button(
                "window-min",
                "−",
                WindowControlArea::Min,
                false,
                |window| window.minimize_window(),
            ))
            .child(window_button(
                "window-max",
                maximize_glyph(window.is_maximized()),
                WindowControlArea::Max,
                false,
                |window| window.zoom_window(),
            ))
            .child(window_button(
                "window-close",
                "×",
                WindowControlArea::Close,
                true,
                |window| window.remove_window(),
            ));
        row
    }

    /// Home: the week total first, then quick add, then projects, then recent
    /// entries. Everything below the total is deliberately quieter.
    fn render_home(&self) -> impl IntoElement {
        let week = &self.snapshot.week;

        // --- the number, at the top, largest on the screen ---
        let hero = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .px_6()
            .pt_6()
            .pb_4()
            .child(
                // An explicit size rather than a step from the scale: the
                // largest step (text_3xl) was still smaller than this number
                // is supposed to be. It has to be the first thing you see.
                div()
                    .text_size(px(72.))
                    .text_color(rgb(0xffffff))
                    .child(timetrack_core::format_duration(week.total_ms)),
            )
            .child(div().text_sm().text_color(rgb(0x7a7a90)).child(format!(
                "this week · {} {}",
                week.entry_count,
                if week.entry_count == 1 {
                    "entry"
                } else {
                    "entries"
                }
            )));

        // A day over 24h is legal data, but it usually means a mistyped
        // interval, so it is worth saying (REQUIREMENTS §4). Built as a div
        // rather than an Option because `child` takes an `IntoElement`.
        let warning = div()
            .px_6()
            .pb_1()
            .text_sm()
            .text_color(rgb(0xd29922))
            .child(if self.snapshot.over_24h_days.is_empty() {
                String::new()
            } else {
                format!(
                    "⚠ {} day(s) total more than 24h",
                    self.snapshot.over_24h_days.len()
                )
            });

        div()
            .id("home-scroll")
            .flex_1()
            .flex_col()
            .overflow_y_scroll()
            .child(hero)
            .child(warning)
            .child(self.render_quick_add())
            .child(self.render_week_by_project())
            .child(self.render_entries())
    }

    /// The prompt panels, drawn above the tab content.
    ///
    /// They live here, in the shared wrapper, rather than inside
    /// `render_home`: a prompt can be opened from any tab (the shortcuts are
    /// global), so drawing it only on Home produced a modal that was never
    /// painted and that took the keyboard anyway.
    ///
    /// Which panel is drawn is the same question the key handler asks
    /// (`open_prompt`), so the two cannot drift apart.
    fn render_prompts(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .when_some(self.dialog.as_ref(), |el, dlg| {
                el.child(self.render_entry_dialog(dlg))
            })
            .when_some(self.project_prompt.as_ref(), |el, prompt| {
                el.child(self.render_project_prompt(prompt))
            })
            .when_some(self.split_prompt.as_ref(), |el, prompt| {
                el.child(self.render_split_prompt(prompt))
            })
            .when_some(self.merge_confirm.as_ref(), |el, confirm| {
                el.child(self.render_merge_confirm(confirm))
            })
            .when_some(self.delete_confirm.as_ref(), |el, confirm| {
                el.child(self.render_delete_confirm(confirm))
            })
    }

    /// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
    ///
    /// A bordered panel at the top of Home, above the week total: while it is
    /// open it owns the keyboard (see `on_dialog_key`), so it sits where the
    /// eye already is rather than competing from below. Keyboard-driven like
    /// everything else here — no mouse targets, no toolkit text field.
    fn render_entry_dialog(&self, dlg: &EntryDialog) -> impl IntoElement {
        let tz = snapshot_tz(&self.snapshot);
        let title = match &dlg.edit_id {
            None => format!("NEW ENTRY → {}", dlg.project_name),
            Some(id) => format!("EDIT ENTRY {id} · {}", dlg.project_name),
        };
        let field_row = |label: &str, value: &str, hint: &str, focused: bool| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .child(
                    div()
                        .w(px(110.))
                        .text_color(if focused {
                            rgb(0xffffff)
                        } else {
                            rgb(0x7a7a90)
                        })
                        .child(format!("{} {label}", if focused { ">" } else { " " })),
                )
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(if focused {
                            rgba(0x2a2a3aff)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_color(rgb(0xe6e6f0))
                        .child(format!("{value}{}", if focused { "_" } else { "" })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x6a6a80))
                        .child(hint.to_string()),
                )
        };

        let mut panel = div()
            .mx_6()
            .mt_3()
            .mb_1()
            .px_4()
            .py_3()
            .rounded_md()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a26))
            .child(div().text_color(rgb(0x7fd3ff)).child(title))
            .child(field_row(
                "what",
                &dlg.description,
                "optional note",
                dlg.field == DialogField::Description,
            ))
            .child(field_row(
                "start",
                &dlg.start,
                &dlg.start_hint(&tz),
                dlg.field == DialogField::Start,
            ))
            .child(field_row(
                "end",
                &dlg.end,
                &dlg.end_hint(&tz),
                dlg.field == DialogField::End,
            ))
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child("type to edit · tab next field · enter save · esc cancel"),
            );
        if let Some(err) = &dlg.error {
            panel = panel.child(div().pt_1().text_color(rgb(0xd29922)).child(err.clone()));
        }
        panel
    }

    /// The split prompt: one moment field, same panel styling as the entry
    /// dialog, above the week total where the eye already is.
    fn render_split_prompt(&self, prompt: &SplitPrompt) -> impl IntoElement {
        let mut panel = div()
            .mx_6()
            .mt_3()
            .mb_1()
            .px_4()
            .py_3()
            .rounded_md()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a26))
            .child(
                div()
                    .text_color(rgb(0x7fd3ff))
                    .child(format!("SPLIT ENTRY {}", prompt.entry_id)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(div().w(px(110.)).text_color(rgb(0xffffff)).child("> at"))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(0x2a2a3aff))
                            .text_color(rgb(0xe6e6f0))
                            .child(format!("{}_", prompt.at)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x6a6a80))
                            .child("(-90, 90, now, HH:MM)"),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child("type a moment · enter split · esc cancel"),
            );
        if let Some(err) = &prompt.error {
            panel = panel.child(div().pt_1().text_color(rgb(0xd29922)).child(err.clone()));
        }
        panel
    }

    /// The merge confirmation banner: what would merge and how the total
    /// changes, computed when the confirmation opened. The amount is the
    /// point (§8): merging overlapping entries shrinks the total, and the
    /// user states the overlap was a mistake by confirming.
    fn render_merge_confirm(&self, confirm: &MergeConfirm) -> impl IntoElement {
        div()
            .mx_6()
            .mt_3()
            .mb_1()
            .px_4()
            .py_3()
            .rounded_md()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a26))
            .child(
                div()
                    .text_color(rgb(0x7fd3ff))
                    .child(confirm.summary.clone()),
            )
    }

    /// The delete confirmation banner: exactly which entry would go, so a
    /// destructive action is never one keystroke with no reading step.
    fn render_delete_confirm(&self, confirm: &DeleteConfirm) -> impl IntoElement {
        div()
            .mx_6()
            .mt_3()
            .mb_1()
            .px_4()
            .py_3()
            .rounded_md()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a26))
            .child(
                div()
                    .text_color(rgb(0x7fd3ff))
                    .child(confirm.summary.clone()),
            )
    }

    /// The project prompt: one name field, same panel styling as the other
    /// prompts, above the week total where the eye already is.
    fn render_project_prompt(&self, prompt: &ProjectPrompt) -> impl IntoElement {
        let title = match &prompt.edit_id {
            None => "NEW PROJECT".to_string(),
            Some(_) => "RENAME PROJECT".to_string(),
        };
        let mut panel = div()
            .mx_6()
            .mt_3()
            .mb_1()
            .px_4()
            .py_3()
            .rounded_md()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a26))
            .child(div().text_color(rgb(0x7fd3ff)).child(title))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(div().w(px(110.)).text_color(rgb(0xffffff)).child("> name"))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(0x2a2a3aff))
                            .text_color(rgb(0xe6e6f0))
                            .child(format!("{}_", prompt.name)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x6a6a80))
                            .child("a short name, e.g. Work"),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child("type a name · enter save · esc cancel"),
            );
        if let Some(err) = &prompt.error {
            panel = panel.child(div().pt_1().text_color(rgb(0xd29922)).child(err.clone()));
        }
        panel
    }

    /// The 5/15/30/60 row, naming the project it will go to.
    fn render_quick_add(&self) -> impl IntoElement {
        let project_id = self.current_project().map(|p| p.id.clone());
        let project_name = self
            .current_project()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "no project".into());

        let mut row = div().flex().items_center().gap_2().px_6().py_3();
        row = row.child(div().text_sm().text_color(rgb(0x7a7a90)).child("add time"));
        for ms in QUICK_ADD_MS {
            let target = project_id.clone();
            row = row.child(
                div()
                    .id(ElementId::from(("quick", ms as u64)))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0x2ea043))
                    .text_color(rgb(0xffffff))
                    // With no project there is nothing to attribute the time
                    // to, so the button is present but inert rather than
                    // silently dropping the tap.
                    .when(target.is_none(), |this| this.cursor_default())
                    .when(target.is_some(), |this| this.cursor_pointer())
                    .child(format!("{}m", ms / 60_000))
                    .when(target.is_some(), move |el| {
                        let entity = self.entity.clone();
                        el.on_click(move |_ev, _window, cx| {
                            let project = target.clone().expect("checked above");
                            if let Some(entity) = entity.clone() {
                                entity.update(cx, |view, cx| {
                                    view.send(Action::QuickAdd { project, ms });
                                    cx.notify();
                                });
                            }
                        })
                    }),
            );
        }
        row.child(
            div()
                .text_sm()
                .text_color(rgb(0x9a9ab0))
                .child(format!("→ {project_name}")),
        )
    }

    /// This week's time per project, longest first.
    fn render_week_by_project(&self) -> impl IntoElement {
        let mut rows: Vec<(&str, i64)> = self
            .active_projects()
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    self.snapshot
                        .week
                        .per_project
                        .get(&p.id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        // Longest first, ties by name, so the order never flickers between
        // refreshes.
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let max_ms = rows.iter().map(|(_, ms)| *ms).max().unwrap_or(0);

        let mut list = div().flex().flex_col();
        if rows.is_empty() {
            list = list.child(
                div()
                    .py_2()
                    .text_color(rgb(0x6a6a80))
                    .child("No projects yet — add one on the Projects tab."),
            );
        }
        for (name, ms) in rows {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .text_color(rgb(0xc8c8d8))
                            .child(name.to_string()),
                    )
                    // A bar proportional to the longest row (§11), so the
                    // distribution reads without reading every number.
                    .child(
                        div()
                            .w(px(PROJECT_BAR_TRACK_PX))
                            .h(px(8.))
                            .rounded_md()
                            .bg(rgb(0x2a2a3a))
                            .child(
                                div()
                                    .w(px(bar_width(ms, max_ms)))
                                    .h_full()
                                    .rounded_md()
                                    .bg(rgb(0x7fd3ff)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(rgb(0x7fd3ff))
                            .child(timetrack_core::format_duration(ms)),
                    ),
            );
        }

        div()
            .child(
                div()
                    .px_6()
                    .pt_2()
                    .text_sm()
                    .text_color(rgb(0x7a7a90))
                    .child("THIS WEEK BY PROJECT"),
            )
            .child(div().px_6().child(list))
    }

    /// Recent entries. Everything is closed, so there is no running marker.
    fn render_entries(&self) -> impl IntoElement {
        if self.snapshot.entries.is_empty() {
            return div()
                .px_6()
                .py_3()
                .text_color(rgb(0x6a6a80))
                .child("Nothing tracked yet — press 1 to add 5 minutes.")
                .into_any_element();
        }

        let project_name = |id: &str| -> String {
            self.snapshot
                .projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string())
        };

        let rows = self.snapshot.entries.iter().enumerate().map(|(i, e)| {
            let selected = i == self.selected;
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_1()
                .bg(if selected {
                    rgba(0x2a2a3aff)
                } else {
                    rgba(0x00000000)
                })
                // A quick-add is marked so it is visibly distinct from
                // hand-entered time, and so `u` reads as "undo that".
                .child(div().w_4().text_color(rgb(0x2ea043)).child(
                    if e.source == timetrack_proto::EntrySource::QuickAdd {
                        "+"
                    } else {
                        " "
                    },
                ))
                .child(
                    div()
                        .w(px(110.))
                        .text_color(rgb(0x7fd3ff))
                        .child(timetrack_core::format_duration(e.duration_ms())),
                )
                .child(
                    div()
                        .w(px(160.))
                        .text_color(rgb(0x9a9ab0))
                        .child(project_name(&e.project_id)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(0xe6e6f0))
                        .child(e.label().to_string()),
                )
        });

        div()
            .px_6()
            .pt_2()
            .pb_1()
            .text_sm()
            .text_color(rgb(0x7a7a90))
            .child("RECENT ENTRIES  (+ = QUICK ADD)")
            .child(div().flex().flex_col().children(rows))
            .into_any_element()
    }

    /// Projects: every project with its week and all-time totals, plus an
    /// archive action. Archived projects stay listed, because their history
    /// still counts (REQUIREMENTS §14).
    fn render_projects(&self) -> impl IntoElement {
        // A "New project" row rather than a dead action: it opens the naming
        // prompt, so every project name is typed -- never invented.
        let new_project = {
            let entity = self.entity.clone();
            div()
                .id("new-project")
                .mx_6()
                .my_3()
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .bg(rgba(0x2ea043ff))
                .text_color(rgb(0xffffff))
                .child("New project")
                .when_some(entity, |this, entity| {
                    this.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.open_project_prompt();
                            cx.notify();
                        });
                    })
                })
        };

        if self.snapshot.projects.is_empty() {
            return div()
                .flex_1()
                .flex_col()
                .px_6()
                .py_4()
                .child(
                    div()
                        .text_color(rgb(0x6a6a80))
                        .child("No projects yet — create one to start tracking time."),
                )
                .child(new_project)
                .into_any_element();
        }

        let rows = self.snapshot.projects.iter().map(|p| {
            // Both columns come from the service's aggregates, never re-summed
            // here: the snapshot may carry only recent entries while the
            // aggregates cover the whole store, and re-summing under-reports
            // a large store's all-time column next to a correct week column.
            let (week, all) = self.snapshot.project_totals(&p.id);
            let id = p.id.clone();
            let name = p.name.clone();
            // The project's colour, if it has one, else the neutral row grey:
            // recolours set from the CLI show up here without another action.
            let dot = rgb(p.colour.unwrap_or(0x7a7a90));
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_2()
                .child(div().w(px(12.)).h(px(12.)).rounded_md().bg(dot))
                .child(
                    div()
                        .flex_1()
                        .text_color(if p.archived {
                            rgb(0x6a6a80)
                        } else {
                            rgb(0xe6e6f0)
                        })
                        .child(if p.archived {
                            format!("{name}  (archived)")
                        } else {
                            name.clone()
                        }),
                )
                .child(
                    div()
                        .w(px(110.))
                        .text_color(rgb(0x7fd3ff))
                        .child(timetrack_core::format_duration(week)),
                )
                .child(
                    div()
                        .w(px(110.))
                        .text_color(rgb(0x7a7a90))
                        .child(timetrack_core::format_duration(all)),
                )
                // Archiving is offered only for live projects; an archived one
                // offers the reverse instead, so archiving is reversible
                // without the CLI.
                .when(!p.archived, {
                    let entity = self.entity.clone();
                    let project_id = id.clone();
                    move |el| {
                        el.child(
                            div()
                                .id(project_button_id("archive", &p.id))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgba(0x3a3a4aff))
                                .text_color(rgb(0xc8c8d8))
                                .child("Archive")
                                .on_click(move |_ev, _window, cx| {
                                    if let Some(entity) = entity.clone() {
                                        entity.update(cx, |view, cx| {
                                            view.send(Action::ArchiveProject(project_id.clone()));
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    }
                })
                .when(p.archived, {
                    let entity = self.entity.clone();
                    let project_id = id.clone();
                    move |el| {
                        el.child(
                            div()
                                .id(project_button_id("unarchive", &p.id))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgba(0x2ea04366))
                                .text_color(rgb(0xc8c8d8))
                                .child("Unarchive")
                                .on_click(move |_ev, _window, cx| {
                                    if let Some(entity) = entity.clone() {
                                        entity.update(cx, |view, cx| {
                                            view.send(Action::UnarchiveProject(project_id.clone()));
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    }
                })
        });

        div()
            .id("projects-scroll")
            .flex_1()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .px_6()
                    .pt_2()
                    .pb_1()
                    .text_sm()
                    .text_color(rgb(0x7a7a90))
                    .child("PROJECTS   (this week · all time)"),
            )
            .child(div().flex().flex_col().children(rows))
            .child(new_project)
            .into_any_element()
    }

    /// One scope pill on the Export tab. Neutral rather than green: green is
    /// the export-action colour the smoke check counts.
    fn render_scope_pill(
        &self,
        scope: timetrack_core::ExportScope,
        id: &'static str,
        label: &'static str,
    ) -> impl IntoElement {
        let selected = self.export_picker.scope == scope;
        let entity = self.entity.clone();
        div()
            .id(ElementId::from(id))
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .bg(if selected {
                rgba(0x2a2a3aff)
            } else {
                rgba(0x00000000)
            })
            .text_color(if selected {
                rgb(0xffffff)
            } else {
                rgb(0x7a7a90)
            })
            .child(label)
            .when_some(entity, move |el, entity| {
                el.on_click(move |_ev, _window, cx| {
                    entity.update(cx, |view, cx| {
                        let tz = snapshot_tz(&view.snapshot);
                        view.export_picker.set_scope(scope, &tz);
                        cx.notify();
                    });
                })
            })
    }

    /// One anchor stepper button. Inert (present but clickless) for all time,
    /// which ignores the anchor. Neutral rather than green, as above.
    fn render_step_button(
        &self,
        id: &'static str,
        label: &'static str,
        delta: i32,
    ) -> impl IntoElement {
        let active = self.export_picker.is_active();
        let entity = self.entity.clone();
        let mut button = div()
            .id(ElementId::from(id))
            .px_3()
            .py_1()
            .rounded_md()
            .bg(rgba(0x3a3a4aff))
            .text_color(if active { rgb(0xc8c8d8) } else { rgb(0x6a6a80) })
            .child(label);
        if active {
            button = button
                .cursor_pointer()
                .when_some(entity, move |el, entity| {
                    el.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.step_export_anchor(delta);
                            cx.notify();
                        });
                    })
                });
        } else {
            button = button.cursor_default();
        }
        button
    }

    /// Export. The service owns aggregation, so what is shown here is exactly
    /// what a CSV would contain, rather than a second aggregation that could
    /// disagree with the totals on Home.
    fn render_export(&self) -> impl IntoElement {
        let mut body = div()
            .id("export-scroll")
            .flex_1()
            .flex_col()
            .px_6()
            .py_2()
            .overflow_y_scroll();

        body = body.child(div().text_sm().text_color(rgb(0x7a7a90)).child("EXPORT"));
        body = body.child(div().pt_2().text_color(rgb(0xc8c8d8)).child(format!(
            "{} entries · {} this week · {} all time",
            self.snapshot.entries.len(),
            timetrack_core::format_duration(self.snapshot.week.total_ms),
            timetrack_core::format_duration(self.snapshot.total_ms),
        )));

        // Scope plus anchor: the stepper and the typed day edit one shared
        // anchor, and all time visibly ignores it. The preview and the
        // export buttons below follow this scope plus anchor.
        let tz = snapshot_tz(&self.snapshot);
        let picker_active = self.export_picker.is_active();
        body = body.child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgb(0x7a7a90))
                .child("SCOPE"),
        );
        {
            let mut row = div().flex().gap_2().pt_1();
            for (scope, id, label) in [
                (timetrack_core::ExportScope::Week, "scope-week", "Week"),
                (timetrack_core::ExportScope::Month, "scope-month", "Month"),
                (timetrack_core::ExportScope::All, "scope-all", "All"),
            ] {
                row = row.child(self.render_scope_pill(scope, id, label));
            }
            body = body.child(row);
        }
        body = body.child(
            div()
                .pt_3()
                .text_sm()
                .text_color(rgb(0x7a7a90))
                .child("ANCHOR"),
        );
        {
            let mut row = div().flex().items_center().gap_2().pt_1();
            row = row.child(self.render_step_button("anchor-prev", "< prev", -1));
            row = row.child(
                div()
                    .px_3()
                    .py_1()
                    .text_color(if picker_active {
                        rgb(0xffffff)
                    } else {
                        rgb(0x6a6a80)
                    })
                    .child(self.export_picker.period_label(&tz)),
            );
            row = row.child(self.render_step_button("anchor-next", "next >", 1));
            body = body.child(row);
        }
        {
            let field_text = if self.export_picker.editing {
                format!("{}_", self.export_picker.anchor_text)
            } else {
                self.export_picker.anchor_text.clone()
            };
            let entity = self.entity.clone();
            let mut field = div()
                .id(ElementId::from("anchor-field"))
                .flex_1()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(if self.export_picker.editing {
                    rgba(0x2a2a3aff)
                } else {
                    rgba(0x00000000)
                })
                .text_color(if picker_active {
                    rgb(0xe6e6f0)
                } else {
                    rgb(0x6a6a80)
                })
                .child(format!("day {field_text}"));
            if picker_active {
                field = field.cursor_pointer().when_some(entity, |el, entity| {
                    el.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.export_picker.start_editing();
                            cx.notify();
                        });
                    })
                });
            } else {
                field = field.cursor_default();
            }
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(field)
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x6a6a80))
                            .child("(YYYY-MM-DD)"),
                    ),
            );
        }
        if picker_active {
            body = body.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child("t type a day · [ previous period · ] next period"),
            );
        } else {
            body = body.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child("anchor ignored for all time"),
            );
        }
        if let Some(err) = &self.export_picker.error {
            body = body.child(div().pt_1().text_color(rgb(0xd29922)).child(err.clone()));
        }

        // The columns a CSV export will carry, shown as a header so the shape
        // is visible without opening a file.
        body = body.child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgb(0x7a7a90))
                .child("CSV COLUMNS"),
        );
        body = body.child(
            div()
                .pt_1()
                .text_color(rgb(0x9a9ab0))
                .child(timetrack_core::CSV_HEADER),
        );

        // Export actions: fetch the CSV over D-Bus and write it to a
        // period-stamped name the status line reports. Scopes match the
        // CLI's `timetrack export --scope week|month|all`; each button saves
        // its own scope with the shared anchor (all time ignores it), so the
        // buttons keep working while the preview follows the picker.
        {
            let mut row = div().flex().gap_2().pt_3();
            for (i, button_scope) in [
                timetrack_core::ExportScope::Week,
                timetrack_core::ExportScope::Month,
                timetrack_core::ExportScope::All,
            ]
            .iter()
            .enumerate()
            {
                let entity = self.entity.clone();
                let button_scope = *button_scope;
                let label = format!("Export {}", button_scope.as_str());
                row = row.child(
                    div()
                        .id(ElementId::from(("export", i as u64)))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(rgb(0x2ea043))
                        .text_color(rgb(0xffffff))
                        .child(label)
                        .when_some(entity, move |el, entity| {
                            el.on_click(move |_ev, _window, cx| {
                                entity.update(cx, |view, cx| {
                                    // Everything about the request is computed
                                    // here, from one snapshot and one zone:
                                    // the destination used to be computed at
                                    // render time and captured, so a zone
                                    // that changed between the last paint and
                                    // the click would have put the file in a
                                    // directory named for a different week.
                                    let tz = snapshot_tz(&view.snapshot);
                                    let request = export_request(
                                        button_scope,
                                        view.export_picker.anchor_ms,
                                        now_ms(),
                                        &tz,
                                    );
                                    view.send(Action::ExportCsv(request));
                                    cx.notify();
                                });
                            })
                        }),
                );
            }
            body = body.child(row);
            body = body.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child(self.export_location_text()),
            );
        }

        // A preview of the first rows of the scoped export, so what is seen
        // is what the stakeholder gets: the picker's scope plus anchor,
        // oldest first (all time ignores the anchor).
        body = body.child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgb(0x7a7a90))
                .child("PREVIEW"),
        );
        let preview = export_preview_entries(
            &self.snapshot,
            self.export_picker.scope,
            self.export_picker.anchor_ms,
            &tz,
        );
        for e in preview.iter().take(20) {
            let project = self
                .snapshot
                .projects
                .iter()
                .find(|p| p.id == e.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| e.project_id.clone());
            body = body.child(
                div()
                    .flex()
                    .gap_3()
                    .py_1()
                    .text_color(rgb(0x9a9ab0))
                    .child(e.id.clone())
                    .child(project)
                    .child(timetrack_core::format_duration(e.duration_ms()))
                    .child(e.label().to_string()),
            );
        }
        if preview.len() > 20 {
            body = body.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child(format!("… and {} more", preview.len() - 20)),
            );
        }
        body.into_any_element()
    }

    fn render_footer(&self) -> impl IntoElement {
        if self.service_missing() {
            let Some(this) = self.entity.clone() else {
                return div().into_any_element();
            };
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .px_6()
                .py_3()
                .child(
                    div().text_sm().text_color(rgb(0xc8c8d8)).child(
                        self.status
                            .as_ref()
                            .map(|s| s.text.clone())
                            .unwrap_or_default(),
                    ),
                )
                .child(
                    div()
                        .id("start-service")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(rgb(0x2ea043))
                        .text_color(rgb(0xffffff))
                        .on_click(move |_ev, _window, cx| {
                            this.update(cx, |view, cx| {
                                view.try_start_service();
                                cx.notify();
                            });
                        })
                        .child("Start the service"),
                )
                .into_any_element();
        }
        div()
            .px_6()
            .py_2()
            .text_sm()
            .text_color(rgb(0x6a6a80))
            .child(self.footer())
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timetrack_proto::{ClientError, EntrySource, EntryView, ServiceError};

    #[test]
    fn quick_add_buckets_are_5_15_30_60() {
        // The Home row and the number keys are hardcoded to these, so a change
        // to the core constant must break this test rather than the UI.
        let mins: Vec<i64> = QUICK_ADD_MS.iter().map(|m| m / 60_000).collect();
        assert_eq!(mins, [5, 15, 30, 60]);
    }

    #[test]
    fn there_are_exactly_three_tabs() {
        assert_eq!(Tab::all().len(), 3);
        assert_eq!(Tab::all()[0], Tab::Home);
    }

    #[test]
    fn tab_labels_match_the_requirements() {
        assert_eq!(Tab::Home.label(), "Home");
        assert_eq!(Tab::Projects.label(), "Projects");
        assert_eq!(Tab::Export.label(), "Export");
    }

    #[test]
    fn every_tab_has_a_distinct_label() {
        // Two tabs sharing a label would make the strip ambiguous.
        let labels: Vec<&str> = Tab::all().iter().map(|t| t.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn no_running_state_survives_anywhere() {
        // A regression guard: the stopwatch's running entry must not creep
        // back in, because there is no such thing any more (REQUIREMENTS §7).
        // The snapshot has no `running` field, so there is nothing that could
        // resurrect one: every entry is closed (REQUIREMENTS §7).
        let snap = Snapshot::default();
        assert!(snap.entries.is_empty());
        for e in &snap.entries {
            // Every entry carries a real end, never an Option.
            let _: i64 = e.ended_at;
        }
    }

    fn entry(id: &str, source: EntrySource, start: i64) -> EntryView {
        EntryView {
            id: id.into(),
            project_id: "p1".into(),
            description: id.into(),
            started_at: start,
            ended_at: start + 3_600_000,
            source,
            note: None,
        }
    }

    fn project(id: &str, name: &str) -> ProjectView {
        ProjectView {
            id: id.into(),
            name: name.into(),
            colour: None,
            archived: false,
        }
    }

    fn entry_on(id: &str, project: &str, start: i64, end: i64) -> EntryView {
        EntryView {
            id: id.into(),
            project_id: project.into(),
            description: id.into(),
            started_at: start,
            ended_at: end,
            source: EntrySource::Manual,
            note: None,
        }
    }

    #[test]
    fn undo_targets_the_newest_quick_add() {
        let snap = Snapshot {
            // Newest first, as the service sends them.
            entries: vec![
                entry("e3", EntrySource::QuickAdd, 2_000),
                entry("e2", EntrySource::Manual, 1_000),
                entry("e1", EntrySource::QuickAdd, 0),
            ],
            ..Default::default()
        };
        assert_eq!(newest_quick_add(&snap).as_deref(), Some("e3"));
    }

    #[test]
    fn there_is_nothing_to_undo_without_a_quick_add() {
        let snap = Snapshot {
            entries: vec![entry("e1", EntrySource::Manual, 0)],
            ..Default::default()
        };
        assert_eq!(newest_quick_add(&snap), None);
    }

    // --- the manual-entry dialog ---

    fn new_dialog() -> EntryDialog {
        EntryDialog::new("p1".into(), "Work".into())
    }

    #[test]
    fn a_new_dialog_prefills_end_at_now() {
        // Method 2 is method 1 with "end = now" pre-filled (§5): the common
        // case needs no typing at all.
        let dlg = new_dialog();
        assert_eq!(dlg.start, "-60");
        assert_eq!(dlg.end, "now");
        assert_eq!(dlg.field, DialogField::Description);
    }

    #[test]
    fn typing_appends_and_backspace_deletes() {
        let mut dlg = new_dialog();
        dlg.type_text("smoke");
        assert_eq!(dlg.description, "smoke");
        dlg.backspace();
        assert_eq!(dlg.description, "smok");
    }

    #[test]
    fn tab_cycles_through_the_fields() {
        let mut dlg = new_dialog();
        assert_eq!(dlg.field, DialogField::Description);
        dlg.field = dlg.field.next();
        assert_eq!(dlg.field, DialogField::Start);
        dlg.field = dlg.field.next();
        assert_eq!(dlg.field, DialogField::End);
        dlg.field = dlg.field.next();
        assert_eq!(dlg.field, DialogField::Description);
        dlg.field = dlg.field.prev();
        assert_eq!(dlg.field, DialogField::End);
    }

    #[test]
    fn saving_a_new_dialog_creates_the_interval() {
        let mut dlg = new_dialog();
        dlg.description = "review".into();
        // now = 12:00 UTC; -60 → 11:00, now → 12:00.
        let now = 12 * 3_600_000;
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, now, &utc) {
            SaveOutcome::Create {
                project_id,
                description,
                started_at,
                ended_at,
            } => {
                assert_eq!(project_id, "p1");
                assert_eq!(description, "review");
                assert_eq!(started_at, 11 * 3_600_000);
                assert_eq!(ended_at, now);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn a_backwards_interval_stays_open_with_an_error() {
        let mut dlg = new_dialog();
        dlg.start = "now".into();
        dlg.end = "-60".into();
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, 12 * 3_600_000, &utc) {
            SaveOutcome::Invalid(msg) => assert!(msg.contains("must not end before")),
            other => panic!("expected Invalid, got {other:?}"),
        }
        // And the dialog keeps the error for the panel to show.
        dlg.error = Some("x".into());
        dlg.field = dlg.field.next();
        assert_eq!(dlg.error, Some("x".into()));
    }

    #[test]
    fn typing_clears_a_stale_error() {
        let mut dlg = new_dialog();
        dlg.error = Some("bad".into());
        dlg.type_text("x");
        assert_eq!(dlg.error, None);
    }

    #[test]
    fn an_untouched_edit_dialog_is_a_no_op() {
        let dlg = EntryDialog::edit("e1".into(), "Work".into(), "kept".into(), 0, 3_600_000);
        let utc = timetrack_core::Tz::utc();
        assert_eq!(
            resolve_save(&dlg, 12 * 3_600_000, &utc),
            SaveOutcome::NoChange
        );
    }

    #[test]
    fn empty_times_keep_the_current_endpoints() {
        // The dialog form of `shorten`: only the typed endpoint moves, and
        // the entry is never at risk.
        let mut dlg = EntryDialog::edit("e1".into(), "Work".into(), "kept".into(), 0, 3_600_000);
        dlg.end = "-60".into();
        let now = 12 * 3_600_000;
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, now, &utc) {
            SaveOutcome::Update {
                id,
                started_at,
                ended_at,
                description,
            } => {
                assert_eq!(id, "e1");
                assert_eq!(started_at, 0, "untouched start is kept");
                assert_eq!(ended_at, now - 3_600_000);
                assert_eq!(description, None, "untouched text sends no SetText");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn rewritten_text_sends_a_description_update() {
        let mut dlg = EntryDialog::edit("e1".into(), "Work".into(), "typo".into(), 0, 3_600_000);
        dlg.description = "review".into();
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, 12 * 3_600_000, &utc) {
            SaveOutcome::Update { description, .. } => {
                assert_eq!(description.as_deref(), Some("review"))
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn the_keep_hints_name_the_current_times() {
        // 01:00 and 02:00 UTC.
        let dlg = EntryDialog::edit(
            "e1".into(),
            "Work".into(),
            String::new(),
            3_600_000,
            7_200_000,
        );
        let utc = timetrack_core::Tz::utc();
        assert_eq!(dlg.start_hint(&utc), "(empty keeps 01:00)");
        assert_eq!(dlg.end_hint(&utc), "(empty keeps 02:00)");
    }

    #[test]
    fn the_keep_hints_use_the_named_zone() {
        // 22:30 UTC is 00:30 the next day in Rome (summer, +2).
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let dlg = EntryDialog::edit(
            "e1".into(),
            "Work".into(),
            String::new(),
            1_783_290_600_000,
            1_783_290_600_000 + 3_600_000,
        );
        assert_eq!(dlg.start_hint(&rome), "(empty keeps 00:30)");
    }

    // --- typed status kinds (no string parsing) ---

    fn status_for(error: &ClientError) -> StatusMsg {
        StatusMsg::from(error)
    }

    #[test]
    fn no_service_offers_to_start_it() {
        let note = status_for(&ClientError::NoService);
        assert_eq!(note.kind, StatusKind::NoService);
        assert!(
            note.text.contains("not running"),
            "must keep the start-service wording: {}",
            note.text
        );
    }

    #[test]
    fn version_mismatch_names_the_fix() {
        let note = status_for(&ClientError::VersionMismatch {
            name: "org.freedesktop.DBus.Error.UnknownInterface".into(),
        });
        assert_eq!(note.kind, StatusKind::VersionMismatch);
        assert!(
            note.text.contains("different versions"),
            "must keep the reinstall wording: {}",
            note.text
        );
    }

    #[test]
    fn refusals_show_the_services_own_wording() {
        // Backward-compatible strings, dispatched by variant.
        let note = status_for(&ClientError::Service(ServiceError::Rule(
            timetrack_core::RuleError::NotQuickAdd("e1".into()),
        )));
        assert_eq!(note.kind, StatusKind::Service);
        assert_eq!(
            note.text,
            "entry 'e1' was not created by a quick add, so it cannot be undone"
        );
    }

    #[test]
    fn hostile_names_never_steer_dispatch() {
        // A project literally named after an error state must still land in
        // the Service bucket: dispatch keys on the variant, never on the
        // text it happens to contain.
        for name in [
            "org.freedesktop.DBus.Error.UnknownInterface",
            "the service is not running, honestly",
            "ServiceUnknown",
        ] {
            let note = status_for(&ClientError::Service(ServiceError::Rule(
                timetrack_core::RuleError::DuplicateProject(name.into()),
            )));
            assert_eq!(
                note.kind,
                StatusKind::Service,
                "project name {name:?} must not steer dispatch"
            );
            assert!(
                note.text.contains(name),
                "wording must survive: {}",
                note.text
            );
        }
    }

    #[test]
    fn transport_failures_are_local_notes() {
        let note = status_for(&ClientError::Transport("d-bus wobble".into()));
        assert_eq!(note.kind, StatusKind::Local);
        assert_eq!(note.text, "d-bus wobble");
    }

    // --- split prompt ---

    fn split_prompt(at: &str) -> SplitPrompt {
        SplitPrompt {
            entry_id: "e1".into(),
            started_at: 0,
            ended_at: 3_600_000,
            at: at.into(),
            error: None,
        }
    }

    #[test]
    fn split_resolves_an_interior_moment() {
        let utc = timetrack_core::Tz::utc();
        // now = 00:45 on day 0; -15 lands at 00:30, inside [0h, 1h].
        let now = 45 * 60_000;
        assert_eq!(
            resolve_split(&split_prompt("-15"), now, &utc),
            Ok(30 * 60_000)
        );
    }

    #[test]
    fn split_refuses_empty_garbage_and_outside_points() {
        let utc = timetrack_core::Tz::utc();
        let now = 45 * 60_000;
        assert!(resolve_split(&split_prompt(""), now, &utc).is_err());
        assert!(resolve_split(&split_prompt("soon"), now, &utc).is_err());
        // Endpoints are not interior: the service would refuse these too.
        assert!(resolve_split(&split_prompt("00:00"), now, &utc).is_err());
        let err = resolve_split(&split_prompt("02:00"), now, &utc).unwrap_err();
        assert!(
            err.contains("outside entry 'e1'"),
            "must name the entry: {err}"
        );
    }

    // --- merge confirmation ---

    fn merge_snapshot() -> Snapshot {
        // e1 [0h,1h] and e2 [0.5h,1.5h] overlap on p1; e3 is disjoint on p1;
        // e4 overlaps e1 but lives on p2 and must never join.
        Snapshot {
            entries: vec![
                entry_on("e1", "p1", 0, 3_600_000),
                entry_on("e2", "p1", 1_800_000, 5_400_000),
                entry_on("e3", "p1", 10 * 3_600_000, 11 * 3_600_000),
                entry_on("e4", "p2", 0, 3_600_000),
            ],
            projects: vec![project("p1", "Work"), project("p2", "Home")],
            ..Default::default()
        }
    }

    #[test]
    fn merge_targets_are_the_overlapping_same_project_entries() {
        let snap = merge_snapshot();
        assert_eq!(
            merge_targets(&snap, 0),
            vec!["e1".to_string(), "e2".to_string()]
        );
        // The disjoint entry has nothing to merge with.
        assert!(merge_targets(&snap, 2).is_empty());
        // Nothing selected, nothing offered.
        assert!(merge_targets(&snap, 99).is_empty());
        assert!(merge_targets(&Snapshot::default(), 0).is_empty());
    }

    #[test]
    fn merge_targets_include_touching_entries() {
        let snap = Snapshot {
            entries: vec![entry_on("a", "p1", 0, 100), entry_on("b", "p1", 100, 200)],
            ..Default::default()
        };
        assert_eq!(
            merge_targets(&snap, 0),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn merge_offer_states_count_project_and_shrink() {
        let snap = merge_snapshot();
        let offer = merge_offer(&snap, 0).expect("e1 and e2 overlap");
        assert_eq!(offer.ids, vec!["e1".to_string(), "e2".to_string()]);
        assert!(
            offer.summary.contains("MERGE 2 ENTRIES"),
            "must state the count: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("Work"),
            "must name the project: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("total shrinks by 00:30:00"),
            "must state the shrink (§8): {}",
            offer.summary
        );
    }

    #[test]
    fn merge_offer_is_none_without_targets() {
        let snap = merge_snapshot();
        assert!(merge_offer(&snap, 2).is_none());
    }

    // --- delete confirmation ---

    #[test]
    fn delete_offer_names_the_entry_project_and_duration() {
        let snap = Snapshot {
            entries: vec![EntryView {
                description: "review".into(),
                ..entry_on("e1", "p1", 0, 3_600_000)
            }],
            projects: vec![project("p1", "Work")],
            ..Default::default()
        };
        let offer = delete_offer(&snap, 0).expect("e1 is selected");
        assert_eq!(offer.id, "e1");
        assert!(
            offer.summary.contains("DELETE e1"),
            "must name the entry: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("Work"),
            "must name the project: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("01:00:00"),
            "must state the duration: {}",
            offer.summary
        );
        assert!(
            offer.summary.contains("d to confirm"),
            "must state the confirm key: {}",
            offer.summary
        );
    }

    #[test]
    fn delete_offer_is_none_without_a_selection() {
        assert!(delete_offer(&Snapshot::default(), 0).is_none());
        let snap = Snapshot {
            entries: vec![entry_on("e1", "p1", 0, 3_600_000)],
            projects: vec![project("p1", "Work")],
            ..Default::default()
        };
        assert!(delete_offer(&snap, 99).is_none());
    }

    // --- which prompt is open ---

    #[test]
    fn no_open_prompt_means_no_prompt() {
        assert_eq!(which_prompt(false, false, false, false, false), None);
    }

    #[test]
    fn each_open_prompt_is_the_one_that_owns_the_keyboard() {
        assert_eq!(
            which_prompt(true, false, false, false, false),
            Some(OpenPrompt::Entry)
        );
        assert_eq!(
            which_prompt(false, true, false, false, false),
            Some(OpenPrompt::Project)
        );
        assert_eq!(
            which_prompt(false, false, true, false, false),
            Some(OpenPrompt::Split)
        );
        assert_eq!(
            which_prompt(false, false, false, true, false),
            Some(OpenPrompt::Merge)
        );
        assert_eq!(
            which_prompt(false, false, false, false, true),
            Some(OpenPrompt::Delete)
        );
    }

    #[test]
    fn the_entry_dialog_wins_over_every_other_prompt() {
        // The key handler used to check the prompts one by one, in this
        // order; the shared question has to keep it.
        assert_eq!(
            which_prompt(true, true, true, true, true),
            Some(OpenPrompt::Entry)
        );
        assert_eq!(
            which_prompt(false, true, true, true, true),
            Some(OpenPrompt::Project)
        );
        assert_eq!(
            which_prompt(false, false, true, true, true),
            Some(OpenPrompt::Split)
        );
        assert_eq!(
            which_prompt(false, false, false, true, true),
            Some(OpenPrompt::Merge)
        );
    }

    // --- the project row buttons ---

    #[test]
    fn two_projects_never_share_a_button_element_id() {
        // The old id parsed the numeric suffix out of the project id and
        // fell back to 0 for anything else, so two ids that were not
        // `p<digits>` would have collided.
        assert_ne!(
            project_button_id("archive", "p1"),
            project_button_id("archive", "p2")
        );
        assert_ne!(
            project_button_id("archive", "imported"),
            project_button_id("archive", "other")
        );
        assert_ne!(
            project_button_id("archive", "imported"),
            project_button_id("unarchive", "imported"),
            "the two buttons of one project must differ too"
        );
    }

    #[test]
    fn one_project_always_gets_the_same_button_element_id() {
        assert_eq!(
            project_button_id("archive", "p7"),
            project_button_id("archive", "p7")
        );
    }

    // --- project prompt ---

    fn project_prompt(edit_id: Option<&str>, name: &str) -> ProjectPrompt {
        ProjectPrompt {
            edit_id: edit_id.map(str::to_string),
            name: name.into(),
            error: None,
        }
    }

    #[test]
    fn project_prompt_creates_or_renames() {
        assert_eq!(
            resolve_project_prompt(&project_prompt(None, "Work")),
            Ok(ProjectSave::Create {
                name: "Work".into()
            })
        );
        assert_eq!(
            resolve_project_prompt(&project_prompt(Some("p1"), "Deep Work")),
            Ok(ProjectSave::Update {
                id: "p1".into(),
                name: "Deep Work".into()
            })
        );
    }

    #[test]
    fn project_prompt_trims_and_refuses_empty() {
        assert_eq!(
            resolve_project_prompt(&project_prompt(None, "  Work  ")),
            Ok(ProjectSave::Create {
                name: "Work".into()
            })
        );
        assert!(resolve_project_prompt(&project_prompt(None, "")).is_err());
        assert!(resolve_project_prompt(&project_prompt(Some("p1"), "   ")).is_err());
    }

    // --- tabs and bars (§11) ---

    #[test]
    fn tab_cycles_forward_through_all_three() {
        assert_eq!(cycle_tab(Tab::Home, false), Tab::Projects);
        assert_eq!(cycle_tab(Tab::Projects, false), Tab::Export);
        assert_eq!(cycle_tab(Tab::Export, false), Tab::Home);
    }

    #[test]
    fn tab_cycles_backward_through_all_three() {
        assert_eq!(cycle_tab(Tab::Home, true), Tab::Export);
        assert_eq!(cycle_tab(Tab::Export, true), Tab::Projects);
        assert_eq!(cycle_tab(Tab::Projects, true), Tab::Home);
    }

    #[test]
    fn maximize_button_names_the_action_it_will_take() {
        // A square to expand, nested squares to restore: the button always
        // states what pressing it does, following the window's state.
        assert_eq!(maximize_glyph(false), "□");
        assert_eq!(maximize_glyph(true), "▣");
    }

    #[test]
    fn bars_fill_the_track_proportionally() {
        assert_eq!(bar_width(3_600_000, 3_600_000), PROJECT_BAR_TRACK_PX);
        assert_eq!(bar_width(1_800_000, 3_600_000), PROJECT_BAR_TRACK_PX / 2.0);
        assert_eq!(bar_width(0, 3_600_000), 0.0);
        // No entries (or no time) anywhere: zero width, never NaN.
        assert_eq!(bar_width(0, 0), 0.0);
    }

    // --- export anchor picker (ticket #25) ---

    fn utc_tz() -> timetrack_core::Tz {
        timetrack_core::Tz::utc()
    }

    fn day_anchor(text: &str) -> i64 {
        timetrack_core::parse_anchor_day(text, "anchor", &utc_tz()).unwrap()
    }

    #[test]
    fn anchor_picker_defaults_to_this_week_at_now() {
        // The anchor is now at view creation; week is the default scope, and
        // the typed day mirrors the anchor's local day.
        let now = 1_791_460_800_000; // Thu 8 Oct 2026 12:00 UTC
        let picker = ExportPicker::new(now);
        assert_eq!(picker.scope, timetrack_core::ExportScope::Week);
        assert_eq!(picker.anchor_ms, now);
        assert_eq!(picker.anchor_text, "2026-10-08");
        assert!(picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "2026-W41");
    }

    #[test]
    fn week_stepper_moves_one_week_per_press_across_year_boundary() {
        // 2026-01-01 was a Thursday opening ISO week 2026-W01; one press back
        // lands in 2025-W52, and the label follows the core week id.
        let mut picker = ExportPicker::new(day_anchor("2026-01-01"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W01");
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2025-12-25"));
        assert_eq!(picker.anchor_text, "2025-12-25");
        assert_eq!(picker.period_label(&utc_tz()), "2025-W52");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-01-01"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W01");
    }

    #[test]
    fn month_stepper_moves_one_month_per_press_across_year_boundary() {
        let mut picker = ExportPicker::new(day_anchor("2026-01-15"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W03");
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        assert_eq!(picker.period_label(&utc_tz()), "2026-01");
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2025-12-15"));
        assert_eq!(picker.anchor_text, "2025-12-15");
        assert_eq!(picker.period_label(&utc_tz()), "2025-12");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-01-15"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-01");
    }

    #[test]
    fn month_stepper_clamps_to_short_months() {
        // 2026 is not a leap year, so stepping back from 31 March lands on
        // 28 February rather than spilling into March.
        let mut picker = ExportPicker::new(day_anchor("2026-03-31"));
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-02-28"));
        assert_eq!(picker.anchor_text, "2026-02-28");
    }

    #[test]
    fn typed_day_and_stepper_share_one_anchor() {
        // Typing a day moves the same anchor the stepper moves: after the
        // commit the label follows, and the next press steps from there.
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.start_editing();
        picker.anchor_text = "2026-10-12".to_string();
        assert!(picker.commit(&utc_tz()));
        assert_eq!(picker.anchor_ms, day_anchor("2026-10-12"));
        assert_eq!(picker.anchor_text, "2026-10-12");
        assert_eq!(picker.error, None);
        assert_eq!(picker.period_label(&utc_tz()), "2026-W42");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-10-19"));
        assert_eq!(picker.anchor_text, "2026-10-19");
        assert_eq!(picker.period_label(&utc_tz()), "2026-W43");
    }

    #[test]
    fn bad_day_leaves_the_anchor_alone() {
        let before = day_anchor("2026-10-08");
        let mut picker = ExportPicker::new(before);
        for bad in ["2026-13-01", "2026-02-30", "not-a-day", "2026-10"] {
            picker.start_editing();
            picker.anchor_text = bad.to_string();
            assert!(!picker.commit(&utc_tz()), "{bad:?} must not commit");
            assert_eq!(
                picker.anchor_ms, before,
                "{bad:?} must leave the anchor alone"
            );
            let err = picker.error.clone().expect("an inline error is shown");
            assert!(err.contains("anchor"), "must name the field: {err}");
            assert!(err.contains(bad), "must name the bad day: {err}");
        }
    }

    #[test]
    fn all_time_leaves_the_anchor_controls_inert() {
        // All time ignores the anchor, so the stepper is a no-op and the
        // field never opens there; the anchor itself is kept, not cleared.
        let before = day_anchor("2026-10-08");
        let mut picker = ExportPicker::new(before);
        picker.set_scope(timetrack_core::ExportScope::All, &utc_tz());
        assert!(!picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "all time");
        picker.step(-1, &utc_tz());
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, before, "stepping all time moves nothing");
        picker.start_editing();
        assert!(!picker.editing, "the day field never opens for all time");
        assert_eq!(picker.anchor_ms, before);
    }

    #[test]
    fn switching_scope_keeps_the_anchor() {
        let anchor = day_anchor("2026-09-15");
        let mut picker = ExportPicker::new(anchor);
        assert_eq!(picker.period_label(&utc_tz()), "2026-W38");
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        assert_eq!(picker.anchor_ms, anchor);
        assert_eq!(picker.anchor_text, "2026-09-15");
        assert_eq!(picker.period_label(&utc_tz()), "2026-09");
    }

    #[test]
    fn period_labels_use_the_core_week_and_month_ids() {
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            "2026-W41"
        );
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "2026-09"
        );
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "all time"
        );
        assert_eq!(
            shift_export_anchor(
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-15"),
                1,
                &utc_tz()
            ),
            day_anchor("2026-09-15"),
            "all time ignores the anchor entirely"
        );
    }

    #[test]
    fn week_stepper_stays_at_midday_across_dst() {
        // GUI pin of the core DST fix: stepping a week across Rome's spring
        // forward must land at midday, not 11:00 drifting off the day.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let before = timetrack_core::parse_anchor_day("2026-03-22", "anchor", &rome).unwrap();
        let after = shift_export_anchor(timetrack_core::ExportScope::Week, before, 1, &rome);
        assert_eq!(format_anchor_day(after, &rome), "2026-03-29");
        assert_eq!(timetrack_core::format_local_hm(after, &rome), "12:00");
        let back = shift_export_anchor(timetrack_core::ExportScope::Week, after, -1, &rome);
        assert_eq!(format_anchor_day(back, &rome), "2026-03-22");
        assert_eq!(timetrack_core::format_local_hm(back, &rome), "12:00");
    }

    #[test]
    fn typed_day_refreshes_from_the_snapshot_zone() {
        // The picker opens on UTC (the default snapshot's zone); the first
        // real snapshot re-mirrors the just-opened value, but never a day
        // being typed.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        // 2026-07-05 23:30 UTC is 2026-07-06 in Rome (+2): the UTC mirror
        // shows the 5th, the Rome mirror the 6th.
        let anchor = 1_783_294_200_000;
        assert_eq!(format_anchor_day(anchor, &utc_tz()), "2026-07-05");
        let mut picker = ExportPicker::new(anchor);
        assert_eq!(picker.anchor_text, "2026-07-05");
        picker.refresh_from_snapshot(&rome);
        assert_eq!(picker.anchor_text, "2026-07-06");
        // While typing, the user's text wins.
        picker.start_editing();
        picker.anchor_text = "2026-10-".to_string();
        picker.refresh_from_snapshot(&utc_tz());
        assert_eq!(picker.anchor_text, "2026-10-");
    }

    #[test]
    fn location_line_names_a_portal_grant_path() {
        // After a portal grant the tab names the chosen path it wrote to;
        // fallback saves (and no save yet) keep the home-directory wording.
        assert_eq!(
            export_location_text(Some(("/home/user/chosen.csv", true))),
            "Saved via system dialog: /home/user/chosen.csv"
        );
        assert_eq!(
            export_location_text(Some(("/home/user/timetrack-export-all.csv", false))),
            export_location_label()
        );
        assert_eq!(export_location_text(None), export_location_label());
    }

    // --- scoped preview plus honest interim save (ticket #26) ---

    /// Three entries, newest first as the service sends them: one in
    /// 2026-W40 (30 Sep) and two in 2026-W41 (6 and 7 Oct).
    fn preview_snapshot() -> Snapshot {
        Snapshot {
            entries: vec![
                entry_on(
                    "e3",
                    "p1",
                    day_anchor("2026-10-07"),
                    day_anchor("2026-10-07") + 3_600_000,
                ),
                entry_on(
                    "e2",
                    "p1",
                    day_anchor("2026-10-06"),
                    day_anchor("2026-10-06") + 3_600_000,
                ),
                entry_on(
                    "e1",
                    "p1",
                    day_anchor("2026-09-30"),
                    day_anchor("2026-09-30") + 3_600_000,
                ),
            ],
            ..Default::default()
        }
    }

    fn preview_ids(
        snap: &Snapshot,
        scope: timetrack_core::ExportScope,
        anchor_ms: i64,
    ) -> Vec<String> {
        export_preview_entries(snap, scope, anchor_ms, &utc_tz())
            .iter()
            .map(|e| e.id.clone())
            .collect()
    }

    #[test]
    fn preview_lists_the_scoped_rows_oldest_first() {
        // The service sends newest first; the preview shows what the file
        // will hold: the anchored week's rows oldest first.
        let snap = preview_snapshot();
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08")
            ),
            vec!["e2".to_string(), "e3".to_string()]
        );
    }

    #[test]
    fn preview_changes_with_the_anchor() {
        let snap = preview_snapshot();
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Week,
                day_anchor("2026-09-30")
            ),
            vec!["e1".to_string()]
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-30")
            ),
            vec!["e1".to_string()]
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::Month,
                day_anchor("2026-10-08")
            ),
            vec!["e2".to_string(), "e3".to_string()]
        );
    }

    #[test]
    fn all_time_preview_ignores_the_anchor() {
        // A stale anchor must never narrow a full export, on screen or in
        // the file: both anchors preview every entry oldest first.
        let snap = preview_snapshot();
        let all = vec!["e1".to_string(), "e2".to_string(), "e3".to_string()];
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-30")
            ),
            all
        );
        assert_eq!(
            preview_ids(
                &snap,
                timetrack_core::ExportScope::All,
                day_anchor("2026-10-08")
            ),
            all
        );
    }

    #[test]
    fn interim_save_uses_period_stamped_names() {
        // One file per week or month so weeks never overwrite each other;
        // all time keeps the plain name whatever the anchor is.
        let week = default_export_path(
            timetrack_core::ExportScope::Week,
            day_anchor("2026-10-08"),
            &utc_tz(),
        );
        assert!(
            week.ends_with("timetrack-export-week-2026-W41.csv"),
            "week file carries its week id: {week}"
        );
        let month = default_export_path(
            timetrack_core::ExportScope::Month,
            day_anchor("2026-09-15"),
            &utc_tz(),
        );
        assert!(
            month.ends_with("timetrack-export-month-2026-09.csv"),
            "month file carries its month id: {month}"
        );
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            let all = default_export_path(timetrack_core::ExportScope::All, anchor, &utc_tz());
            assert!(
                all.ends_with("timetrack-export-all.csv"),
                "all time ignores the anchor: {all}"
            );
        }
    }

    #[test]
    fn an_export_request_names_one_period_everywhere() {
        // The destination used to be computed at render time and captured in
        // the click closure, so it could come from a different snapshot than
        // the period the request names. Everything is derived together now.
        let scope = timetrack_core::ExportScope::Week;
        let anchor = day_anchor("2026-10-08");
        // `now` in a different week, so the request names a *chosen* period
        // and takes the anchored call rather than the current-period one.
        let now = day_anchor("2026-11-05");
        let request = export_request(scope, anchor, now, &utc_tz());
        assert_eq!(request.period_id, "2026-W41");
        assert_eq!(request.suggested_name, "timetrack-export-week-2026-W41.csv");
        assert!(
            request.path.ends_with("timetrack-export-week-2026-W41.csv"),
            "the fallback path names the same week: {}",
            request.path
        );
        assert_eq!(
            request.anchor_ms,
            Some(anchor),
            "a chosen period uses the anchored call"
        );
    }

    #[test]
    fn an_export_request_for_all_time_ignores_the_anchor() {
        let anchor = day_anchor("2026-10-08");
        let request = export_request(timetrack_core::ExportScope::All, anchor, anchor, &utc_tz());
        assert_eq!(request.period_id, "all time");
        assert_eq!(request.suggested_name, "timetrack-export-all.csv");
        assert_eq!(request.anchor_ms, None, "all time uses the old call");
    }

    #[test]
    fn interim_location_is_labelled_home_dir() {
        // The app runs unsandboxed, so the fallback writes to the real
        // home: the label must say so until the portal dialog lands.
        let label = export_location_label();
        assert!(label.contains("~/"), "label names the home dir: {label}");
        assert!(
            !label.contains("sandbox"),
            "label never claims a sandbox: {label}"
        );
    }

    #[test]
    fn saved_status_names_count_scope_period_and_name() {
        // End to end of the interim save wording: the period-stamped path's
        // basename plus the picker's period label, in the shared core
        // wording. The literals are the independent truth, not a recompute.
        let scope = timetrack_core::ExportScope::Week;
        let anchor = day_anchor("2026-10-08");
        let path = default_export_path(scope, anchor, &utc_tz());
        let basename = path.rsplit('/').next().unwrap();
        let period = export_period_label(scope, anchor, &utc_tz());
        assert_eq!(
            export_saved_status(2, scope, &period, basename),
            "exported 2 entries (week 2026-W41) to timetrack-export-week-2026-W41.csv"
        );
        let all_path = default_export_path(
            timetrack_core::ExportScope::All,
            day_anchor("2026-10-08"),
            &utc_tz(),
        );
        let all_basename = all_path.rsplit('/').next().unwrap();
        assert_eq!(
            export_saved_status(
                3,
                timetrack_core::ExportScope::All,
                "all time",
                all_basename
            ),
            "exported 3 entries (all time) to timetrack-export-all.csv"
        );
    }

    #[test]
    fn all_time_save_ignores_the_anchor() {
        // All time travels over the old scope-only call whatever the anchor
        // is, so old services keep serving full exports.
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            assert_eq!(
                export_anchor_for_save(
                    timetrack_core::ExportScope::All,
                    anchor,
                    day_anchor("2026-10-08"),
                    &utc_tz()
                ),
                None
            );
        }
    }

    #[test]
    fn current_scope_save_uses_the_old_call() {
        // The old call is anchored at now by the service, so a current
        // anchor needs no new call and keeps working against old services.
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                day_anchor("2026-10-06"),
                &utc_tz()
            ),
            None
        );
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-10-08"),
                day_anchor("2026-10-01"),
                &utc_tz()
            ),
            None
        );
    }

    #[test]
    fn chosen_scope_save_uses_the_anchored_call() {
        // A week or month outside the current one needs the scope-plus-anchor
        // call; against an old service that call refuses with a version
        // message rather than writing a wrong file.
        let anchor = day_anchor("2026-09-30");
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Week,
                anchor,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            Some(anchor)
        );
        assert_eq!(
            export_anchor_for_save(
                timetrack_core::ExportScope::Month,
                anchor,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            Some(anchor)
        );
    }

    // --- portal save dialog (ticket #27) ---

    #[test]
    fn portal_suggests_period_stamped_names() {
        // The system save dialog opens with the period-stamped default name
        // prefilled: the week or month id for scoped exports so one week
        // never silently overwrites the last, the plain name for all time
        // whatever the anchor is. The literals are the independent truth, not
        // a recompute of the core naming.
        assert_eq!(
            export_suggested_name(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            "timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_suggested_name(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "timetrack-export-month-2026-09.csv"
        );
        for anchor in [day_anchor("2026-09-15"), day_anchor("2026-10-08")] {
            assert_eq!(
                export_suggested_name(timetrack_core::ExportScope::All, anchor, &utc_tz()),
                "timetrack-export-all.csv"
            );
        }
    }

    #[test]
    fn confirmed_save_writes_through_the_granted_path() {
        // A confirmed dialog wins over the home-directory fallback: the
        // bytes go to the path the portal granted, never to the fallback.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Granted("/home/user/chosen.csv".to_string()),
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            Some("/home/user/chosen.csv".to_string())
        );
    }

    #[test]
    fn cancelled_save_is_a_quiet_no_op() {
        // Cancelling the dialog resolves to no destination at all: the
        // caller sends the quiet cancel status instead and writes nothing.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Cancelled,
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            None
        );
    }

    #[test]
    fn unavailable_portal_falls_back_to_the_home_dir_path() {
        // No portal (headless CI): the direct home-directory write runs on,
        // so the workflow never breaks where no dialog can open.
        assert_eq!(
            resolve_export_destination(
                PortalOutcome::Unavailable,
                "/home/user/timetrack-export-week-2026-W41.csv",
            ),
            Some("/home/user/timetrack-export-week-2026-W41.csv".to_string())
        );
    }

    #[test]
    fn granted_path_write_lands_bytes_and_reports_status() {
        // A confirmed save writes the already-fetched scoped CSV through the
        // granted path and reports count, scope, week id and file base name
        // in the shared core wording.
        let csv = format!(
            "{}\ne1,p1,review,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\ne2,p1,review,2026-10-07T12:00:00,2026-10-07T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        let dest =
            std::env::temp_dir().join(format!("tt-portal-granted-{}-week.csv", std::process::id()));
        let dest_str = dest.to_string_lossy().into_owned();
        let basename = dest.file_name().unwrap().to_string_lossy().into_owned();
        match export_write_msg(
            &csv,
            &dest_str,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            true,
        ) {
            Msg::ExportSaved {
                status,
                dest: saved_dest,
                via_portal,
            } => {
                assert_eq!(
                    status,
                    format!("exported 2 entries (week 2026-W41) to {basename}")
                );
                assert_eq!(saved_dest, dest_str);
                assert!(via_portal);
            }
            _ => panic!("a granted write reports success"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), csv);
        std::fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn portal_stub_ignores_anything_else() {
        assert_eq!(portal_stub_outcome("bogus", "x.csv"), None);
        assert_eq!(portal_stub_outcome("", "x.csv"), None);
    }

    #[test]
    fn portal_stub_records_the_dialog_name() {
        // Headless-test seam: `record:<file>` logs the requested dialog
        // name and reports the portal unavailable, so the direct
        // fallback runs where no live portal exists.
        let log = std::env::temp_dir().join(format!("tt-portal-stub-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        assert_eq!(
            portal_stub_outcome(
                &format!("record:{}", log.to_string_lossy()),
                "timetrack-export-week-2026-W41.csv",
            ),
            Some(PortalOutcome::Unavailable)
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        std::fs::remove_file(&log).unwrap();
    }

    // --- portal cancel, overwrite, and failure behavior (ticket #28) ---

    #[test]
    fn cancel_reports_a_quiet_status() {
        // Cancelling the save dialog is a quiet no-op status, never an
        // error: changing your mind is not a failure, so this travels as
        // `Msg::Status`, not `Msg::Error`, and writes nothing (the
        // `cancelled_save_is_a_quiet_no_op` pin above proves no destination).
        assert_eq!(export_cancelled_status(), "export cancelled");
        match export_cancel_msg() {
            Msg::Status(text) => assert_eq!(text, "export cancelled"),
            Msg::Error(_) => panic!("cancel must be a quiet status, not an error"),
            _ => panic!("cancel must be a quiet status"),
        }
    }

    #[test]
    fn existing_destination_is_overwritten_without_a_second_prompt() {
        // The portal's own overwrite question already governed an existing
        // destination, so the app writes straight through it: no existence
        // check, no second confirm, just the new bytes plus the saved status.
        let csv = format!(
            "{}\nnew,p1,review,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        let dest = std::env::temp_dir().join(format!(
            "tt-portal-overwrite-{}-week.csv",
            std::process::id()
        ));
        let dest_str = dest.to_string_lossy().into_owned();
        let basename = dest.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(&dest, "old bytes").unwrap();
        match export_write_msg(
            &csv,
            &dest_str,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            true,
        ) {
            Msg::ExportSaved { status, .. } => assert_eq!(
                status,
                format!("exported 1 entries (week 2026-W41) to {basename}")
            ),
            Msg::Error(_) => panic!("an existing destination must be overwritten, not refused"),
            _ => panic!("overwrite must report success"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), csv);
        std::fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn write_failure_is_a_local_error_naming_path_and_reason() {
        // A granted path that cannot be written and a failing direct
        // fallback share one outcome: a local error naming the path plus the
        // reason, never a service or validation failure.
        let dir = std::env::temp_dir().join(format!("tt-portal-fail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.to_string_lossy().into_owned();
        let csv = format!("{}\n", timetrack_core::CSV_HEADER);
        match export_write_msg(
            &csv,
            &dest,
            timetrack_core::ExportScope::Week,
            "2026-W41",
            false,
        ) {
            Msg::Error(note) => {
                assert_eq!(note.kind, StatusKind::Local);
                assert!(
                    note.text.starts_with(&format!("could not write {dest}")),
                    "must name the path first: {}",
                    note.text
                );
                assert!(
                    note.text.contains(": "),
                    "must name the reason too: {}",
                    note.text
                );
                for banned in ["service", "validation", "invalid", "unknown", "version"] {
                    assert!(
                        !note.text.to_ascii_lowercase().contains(banned),
                        "must never read as a service or validation failure ({banned}): {}",
                        note.text
                    );
                }
            }
            Msg::Status(_) => panic!("an unwritable destination must report a local error"),
            _ => panic!("an unwritable destination must report a local error"),
        }
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn portal_stub_cancel_flag_returns_cancelled() {
        // Headless cancel pin (no live portal): with `<log>.cancel` present,
        // the stub still logs the requested dialog name but answers
        // Cancelled, so the quiet no-op path runs without a dialog.
        let log = std::env::temp_dir().join(format!(
            "tt-portal-cancel-{}-{}.log",
            std::process::id(),
            "cancel-flag"
        ));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.cancel", log.to_string_lossy()));
        std::fs::write(format!("{}.cancel", log.to_string_lossy()), "").unwrap();
        assert_eq!(
            portal_stub_outcome(
                &format!("record:{}", log.to_string_lossy()),
                "timetrack-export-week-2026-W41.csv",
            ),
            Some(PortalOutcome::Cancelled)
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.cancel", log.to_string_lossy()));
    }

    #[test]
    fn portal_stub_fail_flag_returns_an_unwritable_grant() {
        // Headless failure pin (no live portal): with `<log>.fail` present,
        // the stub still logs the requested dialog name but grants a path
        // that cannot be written, so the local-error path runs without a
        // dialog.
        let log = std::env::temp_dir().join(format!(
            "tt-portal-fail-{}-{}.log",
            std::process::id(),
            "fail-flag"
        ));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.fail", log.to_string_lossy()));
        std::fs::write(format!("{}.fail", log.to_string_lossy()), "").unwrap();
        match portal_stub_outcome(
            &format!("record:{}", log.to_string_lossy()),
            "timetrack-export-week-2026-W41.csv",
        ) {
            Some(PortalOutcome::Granted(dest)) => {
                assert!(
                    dest.contains("timetrack-export-week-2026-W41.csv"),
                    "granted path must carry the dialog name: {dest}"
                );
                assert!(
                    std::fs::write(&dest, "probe").is_err(),
                    "granted path must be unwritable: {dest}"
                );
            }
            _other => panic!("fail flag must grant an unwritable path"),
        }
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "timetrack-export-week-2026-W41.csv\n"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(format!("{}.fail", log.to_string_lossy()));
    }
}
