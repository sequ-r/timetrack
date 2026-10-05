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
    AsyncApp, Context, ElementId, Entity, FocusHandle, InteractiveElement, IntoElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px, rgb, rgba,
};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::{ProjectView, Snapshot};

/// How often the service is polled for a new snapshot.
const REFRESH: Duration = Duration::from_millis(1000);

/// A snapshot handed from the service thread to the UI thread.
enum Msg {
    Snapshot(Box<Snapshot>),
    Error(String),
    /// A success worth showing (e.g. where an export was written). Snapshots
    /// never clear feedback, so this survives until the next action.
    Status(String),
    /// An action succeeded. Snapshots never clear feedback (they arrive
    /// every second and would make errors unreadable), so success arrives as
    /// its own message that dismisses the previous error.
    ActionOk,
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
    /// Create a project.
    AddProject(String),
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
    /// Export entries as CSV (REQUIREMENTS §10). The service thread fetches
    /// the CSV over D-Bus and writes it to `path`.
    ExportCsv { scope: String, path: String },
    /// Launch the service binary. The GUI is a flatpak and cannot run host
    /// binaries itself, so this asks the session bus to activate it, which
    /// works when a D-Bus activation file is installed on the host.
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
            // indistinguishable from the service being absent.
            let mut last_error: Option<String> = None;
            let client = loop {
                match timetrack_proto::Client::connect().await {
                    Ok(c) => break c,
                    Err(e) => {
                        let text = e.to_string();
                        let hint = if text.contains("UnknownInterface")
                            || text.contains("Unknown interface")
                        {
                            "The GUI and the service are different versions.\n\
                             Reinstall whichever one is older so both speak\n\
                             org.sequ.timetrack.Entries."
                        } else {
                            "the timetrack service is not running. Start it with:\n\
                             timetrack-service &\n\
                             (or install it to a systemd user unit -- see the README)"
                        };
                        if last_error.as_deref() != Some(hint) {
                            last_error = Some(hint.to_string());
                            let _ = tx.send(Msg::Error(format!("{hint}\n\n({text})")));
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
                            report(
                                &tx,
                                client
                                    .quick_add(&project, ms)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::UndoQuickAdd(id)) => {
                            report(
                                &tx,
                                client.undo_quick_add(&id).await.map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::DeleteEntry(id)) => {
                            report(
                                &tx,
                                client.delete_entry(&id).await.map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::ArchiveProject(id)) => {
                            report(
                                &tx,
                                client
                                    .set_archived(&id, true)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::AddProject(name)) => {
                            report(
                                &tx,
                                client
                                    .add_project(&name)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
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
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
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
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::SetText { id, description }) => {
                            report(
                                &tx,
                                client
                                    .set_text(&id, &description)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
                        }
                        Ok(Action::ExportCsv { scope, path }) => {
                            let outcome = match client.export_csv(&scope).await {
                                Ok(csv) => match std::fs::write(&path, &csv) {
                                    Ok(()) => Ok(format!(
                                        "exported {} entries ({scope}) to {path}",
                                        csv.lines().count().saturating_sub(1)
                                    )),
                                    Err(e) => Err(format!("could not write {path}: {e}")),
                                },
                                Err(e) => Err(e.to_string()),
                            };
                            match outcome {
                                Ok(note) => {
                                    let _ = tx.send(Msg::Status(note));
                                }
                                Err(e) => {
                                    let _ = tx.send(Msg::Error(e));
                                }
                            }
                        }
                        Ok(Action::StartService) => {
                            // Calling a method is what triggers D-Bus
                            // activation; `Client::connect` just asks for a
                            // proxy, so make a real call to force the bus to
                            // start the service if it can.
                            report(
                                &tx,
                                client
                                    .snapshot()
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| e.to_string()),
                            );
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
                        if tx.send(Msg::Error(e.to_string())).is_err() {
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

/// Forward a command outcome to the UI, keeping the service's own wording
/// on failure.
///
/// Takes an already-rendered message rather than a `Result`, because `Result`
/// is ambiguous in this crate: `anyhow` exports a one-parameter alias and gpui
/// re-exports its own, so naming either one here is a coin flip.
fn report(tx: &Sender<Msg>, outcome: std::result::Result<(), String>) {
    match outcome {
        Ok(()) => {
            let _ = tx.send(Msg::ActionOk);
        }
        Err(e) => {
            let _ = tx.send(Msg::Error(e));
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
    fn start_hint(&self, offset_ms: i64) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_start, offset_ms)
            ),
        }
    }

    /// Label for the end row, as above.
    fn end_hint(&self, offset_ms: i64) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_end, offset_ms)
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

fn resolve_save(dlg: &EntryDialog, now: i64, offset_ms: i64) -> SaveOutcome {
    match &dlg.edit_id {
        None => {
            if dlg.start.trim().is_empty() {
                return SaveOutcome::Invalid("start cannot be empty".to_string());
            }
            if dlg.end.trim().is_empty() {
                return SaveOutcome::Invalid("end cannot be empty".to_string());
            }
            let started_at = match timetrack_core::parse_moment(&dlg.start, "start", now, offset_ms)
            {
                Ok(t) => t,
                Err(e) => return SaveOutcome::Invalid(e.to_string()),
            };
            let ended_at = match timetrack_core::parse_moment(&dlg.end, "end", now, offset_ms) {
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
                match timetrack_core::parse_moment(&dlg.start, "start", now, offset_ms) {
                    Ok(t) => t,
                    Err(e) => return SaveOutcome::Invalid(e.to_string()),
                }
            };
            let ended_at = if dlg.end.trim().is_empty() {
                dlg.orig_end
            } else {
                match timetrack_core::parse_moment(&dlg.end, "end", now, offset_ms) {
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

pub struct TimetrackView {
    rx: Receiver<Msg>,
    actions: Sender<Action>,
    snapshot: Snapshot,
    status: Option<String>,
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
}

/// Where a GUI export lands by default.
///
/// The GUI is a flatpak with no file picker yet, so exports go to a
/// predictable name the status line can report. A portal save dialog is the
/// follow-up; until then this is a working button rather than a dead one.
fn default_export_path(scope: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{home}/timetrack-export-{scope}.csv")
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
        // Set the status only when the text differs: the service thread
        // resends nothing, but snapshots arrive every second and must not
        // count as change on their own.
        let set_status = |status: &mut Option<String>, text: String, changed: &mut bool| {
            if status.as_deref() != Some(text.as_str()) {
                *status = Some(text);
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
                Ok(Msg::Error(e)) => set_status(&mut self.status, e, &mut changed),
                Ok(Msg::Status(note)) => set_status(&mut self.status, note, &mut changed),
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
                        "the service thread stopped".to_string(),
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

    /// Handle a key press. Returns false when the app should quit.
    fn on_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        // Modal: while the entry dialog is open every keystroke belongs to
        // it, including the letters the app shortcuts are bound to.
        if self.dialog.is_some() {
            return self.on_dialog_key(ev);
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

            // Undo removes only the quick-added entry; delete is explicit.
            "u" => match newest_quick_add(&self.snapshot) {
                Some(id) => self.send(Action::UndoQuickAdd(id)),
                None => self.status = Some("nothing to undo".into()),
            },
            "d" => {
                if let Some(e) = self.snapshot.entries.get(self.selected) {
                    let id = e.id.clone();
                    self.send(Action::DeleteEntry(id));
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
                self.status =
                    Some("create a project on the Projects tab before adding time".into());
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
            None => self.status = Some("nothing to edit".into()),
        }
    }

    /// Validate the dialog and send the action(s). Stays open with an error
    /// on invalid input; closes on success or when nothing changed.
    fn dialog_save(&mut self) {
        let Some(dlg) = self.dialog.as_ref() else {
            return;
        };
        let (orig_start, orig_end) = (dlg.orig_start, dlg.orig_end);
        match resolve_save(dlg, now_ms(), self.snapshot.local_offset_ms) {
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
                self.status =
                    Some("create a project on the Projects tab before adding time".into());
            }
        }
    }

    /// Whether the service has not been seen yet, in which case the window
    /// offers to start it instead of just complaining.
    fn service_missing(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.contains("not running"))
    }

    fn try_start_service(&self) {
        self.send(Action::StartService);
    }

    fn footer(&self) -> String {
        self.status.clone().unwrap_or_else(|| {
            if self.dialog.is_some() {
                "type to edit · tab next field · enter save · esc cancel".to_string()
            } else {
                "1-4 quick add · a add · e edit · u undo · d delete · j/k move · h/l project · q quit"
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

        // The tab strip is the only chrome above the content, and it is chrome
        // rather than data -- the week total is still the first *number* on
        // screen and nothing of substance sits above it.
        //
        // The `.id()` and `.track_focus()` are load-bearing, not decoration:
        // gpui dispatches key events along the ancestor path of the focused
        // node, and an element with no id is not in the dispatch tree at all.
        div()
            .id("root")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x11111b))
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
            .child(self.render_tabs())
            .child(match self.tab {
                Tab::Home => self.render_home().into_any_element(),
                Tab::Projects => self.render_projects().into_any_element(),
                Tab::Export => self.render_export().into_any_element(),
            })
            .child(self.render_footer())
    }
}

impl TimetrackView {
    /// The tab strip.
    fn render_tabs(&self) -> impl IntoElement {
        let mut row = div().flex().gap_1().px_6().pt_3();
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
            .when_some(self.dialog.clone(), |el, dlg| {
                el.child(self.render_entry_dialog(&dlg))
            })
            .child(hero)
            .child(warning)
            .child(self.render_quick_add())
            .child(self.render_week_by_project())
            .child(self.render_entries())
    }

    /// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
    ///
    /// A bordered panel at the top of Home, above the week total: while it is
    /// open it owns the keyboard (see `on_dialog_key`), so it sits where the
    /// eye already is rather than competing from below. Keyboard-driven like
    /// everything else here — no mouse targets, no toolkit text field.
    fn render_entry_dialog(&self, dlg: &EntryDialog) -> impl IntoElement {
        let off = self.snapshot.local_offset_ms;
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
                &dlg.start_hint(off),
                dlg.field == DialogField::Start,
            ))
            .child(field_row(
                "end",
                &dlg.end,
                &dlg.end_hint(off),
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
                    .justify_between()
                    .py_1()
                    .child(div().text_color(rgb(0xc8c8d8)).child(name.to_string()))
                    .child(
                        div()
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
        // A "New project" row rather than a dead action: creating a project is
        // the one thing on this tab with no other route into it.
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
                            // A typed name needs a text field, which this tab
                            // does not have yet. A dated default the user can
                            // rename is a working button rather than a dead
                            // one, and it is removed once the field lands.
                            let name = format!("Project {}", view.snapshot.projects.len() + 1);
                            view.send(Action::AddProject(name));
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
            let week = self
                .snapshot
                .week
                .per_project
                .get(&p.id)
                .copied()
                .unwrap_or(0);
            // The all-time total is summed from the entries rather than taken
            // from the service, because the service's snapshot carries week
            // and month only. Same rule either way: a sum of durations.
            let all = self
                .snapshot
                .entries
                .iter()
                .filter(|e| e.project_id == p.id)
                .map(|e| e.duration_ms())
                .sum::<i64>();
            let id = p.id.clone();
            let name = p.name.clone();
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_2()
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
                // has nothing left to hide.
                .when(!p.archived, {
                    let entity = self.entity.clone();
                    move |el| {
                        el.child(
                            div()
                                // Project ids are `p1`, `p2`, ... so the numeric
                                // suffix is unique within the store and a valid id.
                                .id(ElementId::from((
                                    "archive",
                                    p.id.trim_start_matches('p').parse::<u64>().unwrap_or(0),
                                )))
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
                                            view.send(Action::ArchiveProject(id.clone()));
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
        // predictable path the status line reports. Scopes match the CLI's
        // `timetrack export --scope week|month|all`.
        {
            let mut row = div().flex().gap_2().pt_3();
            for (i, scope) in ["week", "month", "all"].iter().enumerate() {
                let entity = self.entity.clone();
                let path = default_export_path(scope);
                let label = format!("Export {scope}");
                let scope = scope.to_string();
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
                                let scope = scope.clone();
                                let path = path.clone();
                                entity.update(cx, |view, cx| {
                                    view.send(Action::ExportCsv { scope, path });
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
                    .child("Writes ~/timetrack-export-<week|month|all>.csv"),
            );
        }

        // A preview of the first few rows, so the export can be sanity-checked
        // on screen.
        body = body.child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgb(0x7a7a90))
                .child("PREVIEW"),
        );
        for e in self.snapshot.entries.iter().take(20) {
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
        if self.snapshot.entries.len() > 20 {
            body = body.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgb(0x6a6a80))
                    .child(format!("… and {} more", self.snapshot.entries.len() - 20)),
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
                    div()
                        .text_sm()
                        .text_color(rgb(0xc8c8d8))
                        .child(self.status.clone().unwrap_or_default()),
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
    use timetrack_proto::{EntrySource, EntryView};

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

    #[test]
    fn undo_targets_the_newest_quick_add() {
        let mut snap = Snapshot::default();
        // Newest first, as the service sends them.
        snap.entries = vec![
            entry("e3", EntrySource::QuickAdd, 2_000),
            entry("e2", EntrySource::Manual, 1_000),
            entry("e1", EntrySource::QuickAdd, 0),
        ];
        assert_eq!(newest_quick_add(&snap).as_deref(), Some("e3"));
    }

    #[test]
    fn there_is_nothing_to_undo_without_a_quick_add() {
        let mut snap = Snapshot::default();
        snap.entries = vec![entry("e1", EntrySource::Manual, 0)];
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
        match resolve_save(&dlg, now, 0) {
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
        match resolve_save(&dlg, 12 * 3_600_000, 0) {
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
        assert_eq!(resolve_save(&dlg, 12 * 3_600_000, 0), SaveOutcome::NoChange);
    }

    #[test]
    fn empty_times_keep_the_current_endpoints() {
        // The dialog form of `shorten`: only the typed endpoint moves, and
        // the entry is never at risk.
        let mut dlg = EntryDialog::edit("e1".into(), "Work".into(), "kept".into(), 0, 3_600_000);
        dlg.end = "-60".into();
        let now = 12 * 3_600_000;
        match resolve_save(&dlg, now, 0) {
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
        match resolve_save(&dlg, 12 * 3_600_000, 0) {
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
        assert_eq!(dlg.start_hint(0), "(empty keeps 01:00)");
        assert_eq!(dlg.end_hint(0), "(empty keeps 02:00)");
    }
}
