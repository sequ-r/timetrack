/* cli/tui.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The ratatui front end.
//!
//! The service is the source of truth, so the TUI holds no state of its
//! own: it refreshes when the service announces a change (#103), with a
//! slow fallback poll behind the signal. Nothing is running any more, so
//! there is no local ticking to interpolate: every number on screen came
//! from the last snapshot.
//!
//! # Layout mirrors the GUI's Home tab
//!
//! The week total is the largest thing on screen and nothing is above it
//! (REQUIREMENTS §7), then the quick-add row, then this week's projects,
//! then the recent entries. The GUI and the CLI showing the same hierarchy in
//! the same order is deliberate.
//!
//! # The calls happen on their own thread
//!
//! The UI thread never waits for the bus: every call happens on a service
//! thread and the outcome comes back as a message. A service that accepts
//! the connection and then goes quiet therefore costs a line in the footer
//! instead of a frozen screen -- and `q` keeps quitting while a call is
//! outstanding (issue #43).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use std::io::{Stdout, stdout};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::{Client, ClientError, EntryView, ProjectView, Snapshot};

/// The fallback refresh cadence (#103): the service's `Changed` signal
/// wakes the service thread the moment a mutation persists, so this
/// slow tick only covers an old (signalless) service, a missed signal,
/// and noticing that the service has gone away.
const TICK: Duration = Duration::from_secs(30);

/// How often to check for a keypress while idle.
const POLL: Duration = Duration::from_millis(100);

struct App {
    /// Where the UI's commands go.
    ///
    /// The calls themselves happen on the service thread, so the UI never
    /// waits for the bus: a service that accepts the connection and then
    /// goes quiet cannot freeze the TUI, and `q` keeps working while a call
    /// is outstanding (issue #43).
    requests: Sender<Request>,
    snapshot: Snapshot,
    selected: usize,
    /// Index into the *active* projects for the project quick-adds go to.
    project_cursor: usize,
    /// The entry a pending delete would remove, if `d` is waiting for its
    /// second key.
    delete_confirm: Option<DeleteConfirm>,
    /// The text prompt being filled in, if one is open (#106). While it
    /// is, it owns the keyboard: its keys are text, not commands.
    prompt: Option<Prompt>,
    /// A project just created through the prompt: the next snapshot puts
    /// the picker cursor on it, so the time that prompted creating it can
    /// be recorded straight away.
    pending_project_id: Option<String>,
    /// A transient message shown in the footer.
    status: Option<String>,
    should_quit: bool,
}

/// A pending delete confirmation: the entry that would go, and the one-line
/// summary the footer shows while it waits.
///
/// The summary names the project and the duration, so `d` can never remove
/// time as a side effect of reaching for another key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteConfirm {
    /// The entry that would be removed.
    id: String,
    /// What the footer says while the confirmation is pending.
    summary: String,
}

/// Which prompt is open (#106): the three things the TUI could not do at
/// all before — record a hand-entered entry, create a project, and export
/// a chosen period — each a short form over a single text-field editor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptKind {
    /// `p`: one field, the new project's name.
    NewProject,
    /// `e`: start, end (empty = now), description, for the project that
    /// was current when the form opened.
    ManualEntry,
    /// `7`/`8`: the anchor day of the week or month to export.
    ExportAnchor { scope: timetrack_core::ExportScope },
}

/// One labelled line of a [`Prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptField {
    label: &'static str,
    value: String,
}

/// A text form being filled in. Editing is deliberately the smallest
/// thing that works — append, backspace, Tab/Enter to move, Esc to
/// leave — matching how far the GUI's single-line fields went before
/// #109, and resolvable by pure functions so the rules are testable
/// without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prompt {
    kind: PromptKind,
    title: String,
    fields: Vec<PromptField>,
    active: usize,
    error: Option<String>,
    /// The project a manual entry goes to, fixed at open time so a
    /// refresh mid-typing cannot silently re-aim it.
    project_id: Option<String>,
}

impl Prompt {
    fn new_project() -> Self {
        Prompt {
            kind: PromptKind::NewProject,
            title: "New project".into(),
            fields: vec![PromptField {
                label: "name",
                value: String::new(),
            }],
            active: 0,
            error: None,
            project_id: None,
        }
    }

    fn manual_entry(project_id: String, project_name: &str) -> Self {
        Prompt {
            kind: PromptKind::ManualEntry,
            title: format!("New entry for {project_name}"),
            fields: vec![
                PromptField {
                    label: "start (-90 or 09:30)",
                    value: "-30".into(),
                },
                PromptField {
                    label: "end (empty = now)",
                    value: String::new(),
                },
                PromptField {
                    label: "description",
                    value: String::new(),
                },
            ],
            active: 0,
            error: None,
            project_id: Some(project_id),
        }
    }

    fn export_anchor(scope: timetrack_core::ExportScope, today: String) -> Self {
        Prompt {
            kind: PromptKind::ExportAnchor { scope },
            title: format!("Export {} containing…", scope.as_str()),
            fields: vec![PromptField {
                label: "day (YYYY-MM-DD)",
                value: today,
            }],
            active: 0,
            error: None,
            project_id: None,
        }
    }

    /// The trimmed value of field `i` (empty past the end, defensively).
    fn field(&self, i: usize) -> &str {
        self.fields.get(i).map(|f| f.value.trim()).unwrap_or("")
    }
}

/// A hand-entered entry as the form resolved it: instants already parsed
/// in the service's zone, ready for the service thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManualEntry {
    description: String,
    started_at: i64,
    ended_at: i64,
}

/// Resolve a manual-entry form. Pure over an explicit clock and the
/// service's zone (#102), in the spellings every client shares: `-90`
/// minutes ago or `09:30` today for both ends, empty end = now.
fn resolve_manual_entry(
    start: &str,
    end: &str,
    description: &str,
    now_ms: i64,
    tz: &timetrack_core::Tz,
) -> Result<ManualEntry, String> {
    if start.is_empty() {
        return Err("start: type a time (-90 or 09:30)".into());
    }
    let started_at =
        timetrack_core::parse_moment(start, "start", now_ms, tz).map_err(|e| e.to_string())?;
    let ended_at = if end.is_empty() {
        now_ms
    } else {
        timetrack_core::parse_moment(end, "end", now_ms, tz).map_err(|e| e.to_string())?
    };
    if ended_at <= started_at {
        return Err("end must be after start".into());
    }
    Ok(ManualEntry {
        description: description.to_string(),
        started_at,
        ended_at,
    })
}

/// Resolve an export-anchor form: the anchor instant (local midday of
/// the typed day, like every client) and the file it should be saved as,
/// from the core's one naming table.
fn resolve_export(
    day: &str,
    scope: timetrack_core::ExportScope,
    tz: &timetrack_core::Tz,
) -> Result<(i64, String), String> {
    let anchor_ms =
        timetrack_core::parse_anchor_day(day, "anchor day", tz).map_err(|e| e.to_string())?;
    let file = timetrack_core::default_export_filename(scope, anchor_ms, tz);
    Ok((anchor_ms, file))
}

/// The zone typed local times are parsed in: the service's, from the
/// snapshot, strictly (#102). An unusable name is an error the form
/// shows, never a guessed zone.
fn snapshot_tz(snap: &Snapshot) -> Result<timetrack_core::Tz, String> {
    timetrack_core::Tz::from_snapshot(&snap.tz).map_err(|e| e.to_string())
}

/// The wall clock in epoch milliseconds, for parsing relative moments.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl App {
    /// A fresh UI, with its commands going to the service thread.
    fn new(requests: Sender<Request>) -> Self {
        App {
            requests,
            snapshot: Snapshot::default(),
            selected: 0,
            project_cursor: 0,
            delete_confirm: None,
            prompt: None,
            pending_project_id: None,
            status: None,
            should_quit: false,
        }
    }

    /// Put what the service thread reported on screen.
    ///
    /// A failure never takes the UI down: it lands in the footer as a
    /// message, and the TUI keeps running (issue #43).
    fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Snapshot(s) => {
                self.snapshot = *s;
                // The lists may have shrunk under the cursor.
                if self.selected >= self.snapshot.entries.len() {
                    self.selected = self.snapshot.entries.len().saturating_sub(1);
                }
                let project_count = active_projects(&self.snapshot).len();
                if self.project_cursor >= project_count {
                    self.project_cursor = project_count.saturating_sub(1);
                }
                // A just-created project becomes the picker's project:
                // creating it was presumably so the next entry lands
                // there.
                if let Some(id) = self.pending_project_id.take() {
                    if let Some(at) = active_projects(&self.snapshot)
                        .iter()
                        .position(|p| p.id == id)
                    {
                        self.project_cursor = at;
                    } else {
                        self.pending_project_id = Some(id);
                    }
                }
                // A routine poll must not wipe feedback from the last action
                // ("added 30m", an undo error, ...): the tick fires every
                // second, so clearing here made every message unreadable.
                // Only the poll's own "service unavailable" notice clears on
                // recovery; action feedback survives until the next action.
                if self
                    .status
                    .as_deref()
                    .is_some_and(|m| m.starts_with("service unavailable"))
                {
                    self.status = None;
                }
            }
            Reply::QuickAdded(entry) => {
                self.status = Some(format!(
                    "added {} to {} (u to undo)",
                    timetrack_core::format_compact(entry.duration_ms()),
                    self.project_name(&entry.project_id)
                ));
            }
            Reply::DurationAdded(entry) => {
                self.status = Some(format!(
                    "added {} to {}",
                    timetrack_core::format_compact(entry.duration_ms()),
                    self.project_name(&entry.project_id)
                ));
            }
            Reply::EntryAdded(entry) => {
                self.status = Some(format!(
                    "added {} to {}",
                    timetrack_core::format_compact(entry.duration_ms()),
                    self.project_name(&entry.project_id)
                ));
            }
            Reply::ProjectAdded(project) => {
                self.pending_project_id = Some(project.id.clone());
                self.status = Some(format!(
                    "created project {}",
                    timetrack_core::sanitize_display(&project.name)
                ));
            }
            Reply::Exported { file, rows } => {
                self.status = Some(format!("exported {rows} entries to {file}"));
            }
            Reply::ExportFailed(msg) => self.status = Some(msg),
            Reply::Undone { id } => self.status = Some(format!("undid quick add {id}")),
            Reply::Deleted { id } => self.status = Some(format!("deleted {id}")),
            Reply::NothingToUndo => self.status = Some("nothing to undo".into()),
            Reply::RefreshFailed(e) => self.status = Some(format!("service unavailable: {e}")),
            Reply::ActionFailed(e) => self.status = Some(e.to_string()),
        }
    }

    /// The name of a project, or its id when the snapshot does not have it.
    fn project_name(&self, id: &str) -> String {
        project_name(&self.snapshot, id)
    }

    /// The project quick-add and the new-entry dialogs attribute time to.
    ///
    /// Indexes the *active* projects, not the raw list: archiving hides a
    /// project from the picker, so the cursor must not be able to land on one
    /// (or every index past it would attribute time to the wrong project).
    fn current_project(&self) -> Option<&ProjectView> {
        current_project(&self.snapshot, self.project_cursor)
    }

    /// Ask the service thread to add one of the quick-add buckets.
    ///
    /// The call is not waited for: the UI keeps drawing and keeps reading
    /// keys while the service thinks about it, so a wedged service costs a
    /// status message, not a frozen TUI (issue #43).
    fn quick_add(&mut self, duration_ms: i64) {
        let Some(project) = self.current_project().map(|p| p.id.clone()) else {
            self.status = Some("no project yet -- press p to create one".into());
            return;
        };
        let _ = self.requests.send(Request::QuickAdd {
            project,
            ms: duration_ms,
        });
    }

    /// Ask the service thread to record a duration ending now.
    fn add_duration(&mut self) {
        let Some(project) = self.current_project().map(|p| p.id.clone()) else {
            self.status = Some("no project yet -- press p to create one".into());
            return;
        };
        // Thirty minutes is the middle default and the least destructive
        // thing to add by accident.
        let _ = self.requests.send(Request::AddDuration {
            project,
            ms: 30 * 60_000,
        });
    }

    /// Open the manual-entry form for the current project (#106).
    fn open_manual_entry(&mut self) {
        let Some(project) = self.current_project() else {
            self.status = Some("no project yet -- press p to create one".into());
            return;
        };
        let (id, name) = (
            project.id.clone(),
            timetrack_core::sanitize_display(&project.name),
        );
        self.prompt = Some(Prompt::manual_entry(id, &name));
    }

    /// Open the export-anchor form for a week or month (#106), prefilled
    /// with today in the service's zone — the current period is one
    /// Enter away, a chosen one is an edit of the day.
    fn open_export(&mut self, scope: timetrack_core::ExportScope) {
        match snapshot_tz(&self.snapshot) {
            Ok(tz) => {
                let today = timetrack_core::anchor_day_string(now_ms(), &tz);
                self.prompt = Some(Prompt::export_anchor(scope, today.to_string()));
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// Export everything now (#106): all time has no anchor to choose.
    fn export_all(&mut self) {
        let _ = self.requests.send(Request::Export {
            scope: timetrack_core::ExportScope::All,
            anchor_ms: 0,
            file: timetrack_core::default_export_filename(
                timetrack_core::ExportScope::All,
                0,
                &timetrack_core::Tz::utc(),
            ),
        });
    }

    /// A key while a prompt is open: text and field movement only. The
    /// command keys cannot fire mid-typing — a `q` in a project name is
    /// a letter, and `Esc` is the way out.
    fn prompt_key(&mut self, key: event::KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.prompt = None;
                self.should_quit = true;
            }
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter => {
                let last = self
                    .prompt
                    .as_ref()
                    .is_some_and(|p| p.active + 1 >= p.fields.len());
                if last {
                    self.submit_prompt();
                } else if let Some(p) = self.prompt.as_mut() {
                    p.active += 1;
                }
            }
            KeyCode::Tab => {
                if let Some(p) = self.prompt.as_mut() {
                    p.active = (p.active + 1) % p.fields.len().max(1);
                }
            }
            KeyCode::BackTab => {
                if let Some(p) = self.prompt.as_mut() {
                    p.active = (p.active + p.fields.len().max(1) - 1) % p.fields.len().max(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = self.prompt.as_mut() {
                    if let Some(field) = p.fields.get_mut(p.active) {
                        field.value.pop();
                    }
                    p.error = None;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(p) = self.prompt.as_mut() {
                    if let Some(field) = p.fields.get_mut(p.active) {
                        field.value.push(c);
                    }
                    p.error = None;
                }
            }
            _ => {}
        }
    }

    /// Act on a completed prompt. A resolution failure stays in the form
    /// as its error line (like the GUI dialog staying open); a success
    /// closes it and hands the work to the service thread.
    fn submit_prompt(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let fail = |prompt: &mut Prompt, e: String| prompt.error = Some(e);
        match &prompt.kind {
            PromptKind::NewProject => {
                let name = prompt.field(0);
                if name.is_empty() {
                    let mut prompt = prompt;
                    fail(&mut prompt, "type a project name".into());
                    self.prompt = Some(prompt);
                    return;
                }
                let _ = self.requests.send(Request::AddProject {
                    name: name.to_string(),
                });
            }
            PromptKind::ManualEntry => {
                let tz = match snapshot_tz(&self.snapshot) {
                    Ok(tz) => tz,
                    Err(e) => {
                        let mut prompt = prompt;
                        fail(&mut prompt, e);
                        self.prompt = Some(prompt);
                        return;
                    }
                };
                match resolve_manual_entry(
                    prompt.field(0),
                    prompt.field(1),
                    prompt.field(2),
                    now_ms(),
                    &tz,
                ) {
                    Ok(entry) => {
                        let Some(project) = prompt.project_id.clone() else {
                            return;
                        };
                        let _ = self.requests.send(Request::AddEntry {
                            project,
                            description: entry.description,
                            started_at: entry.started_at,
                            ended_at: entry.ended_at,
                        });
                    }
                    Err(e) => {
                        let mut prompt = prompt;
                        fail(&mut prompt, e);
                        self.prompt = Some(prompt);
                    }
                }
            }
            PromptKind::ExportAnchor { scope } => {
                let scope = *scope;
                let tz = match snapshot_tz(&self.snapshot) {
                    Ok(tz) => tz,
                    Err(e) => {
                        let mut prompt = prompt;
                        fail(&mut prompt, e);
                        self.prompt = Some(prompt);
                        return;
                    }
                };
                match resolve_export(prompt.field(0), scope, &tz) {
                    Ok((anchor_ms, file)) => {
                        let _ = self.requests.send(Request::Export {
                            scope,
                            anchor_ms,
                            file,
                        });
                    }
                    Err(e) => {
                        let mut prompt = prompt;
                        fail(&mut prompt, e);
                        self.prompt = Some(prompt);
                    }
                }
            }
        }
    }

    /// Ask the service thread to undo the last quick add.
    ///
    /// The target is resolved by the thread, not here: a quick add that has
    /// not landed yet is still the one the user means to undo, and the UI
    /// cannot know its id.
    fn undo_quick_add(&mut self) {
        let _ = self.requests.send(Request::UndoQuickAdd);
    }

    /// Ask the service thread to delete an entry.
    fn delete_entry(&mut self, id: String) {
        let _ = self.requests.send(Request::DeleteEntry { id });
    }

    fn on_key(&mut self, key: event::KeyEvent) {
        // A prompt owns the keyboard while it is open (#106), ahead of
        // even the delete confirmation: its keys are text.
        if self.prompt.is_some() {
            self.prompt_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A pending confirmation listens only to its own answers: `d`
        // confirms, `Esc` -- or any other key -- disarms. Nothing else can
        // happen between the two `d`s.
        if self.delete_confirm.is_some() {
            match pending_answer(key.code, ctrl) {
                // `d` falls through to the delete arm below.
                PendingAnswer::Confirm => {}
                PendingAnswer::Quit => {
                    self.delete_confirm = None;
                    self.should_quit = true;
                    return;
                }
                PendingAnswer::Disarm => {
                    self.delete_confirm = None;
                    return;
                }
            }
        }

        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('q') => self.should_quit = true,

            // --- quick add: 5/15/30/60 on the number row (REQUIREMENTS §6) ---
            KeyCode::Char('1') => self.quick_add(QUICK_ADD_MS[0]),
            KeyCode::Char('2') => self.quick_add(QUICK_ADD_MS[1]),
            KeyCode::Char('3') => self.quick_add(QUICK_ADD_MS[2]),
            KeyCode::Char('4') => self.quick_add(QUICK_ADD_MS[3]),

            // --- method 2: a duration ending now ---
            KeyCode::Char('a') => self.add_duration(),

            // --- method 1: a hand-entered interval (the form) ---
            KeyCode::Char('e') => self.open_manual_entry(),

            // --- a new project, so the TUI is usable from empty ---
            KeyCode::Char('p') => self.prompt = Some(Prompt::new_project()),

            // --- exports: chosen week/month via the anchor form, all now ---
            KeyCode::Char('7') => self.open_export(timetrack_core::ExportScope::Week),
            KeyCode::Char('8') => self.open_export(timetrack_core::ExportScope::Month),
            KeyCode::Char('9') => self.export_all(),

            // --- undo: only ever the last quick add ---
            KeyCode::Char('u') => self.undo_quick_add(),

            // --- explicit delete: two keys, like the GUI and the tray ---
            KeyCode::Char('d') => {
                let effect =
                    delete_effect(self.delete_confirm.as_ref(), &self.snapshot, self.selected);
                match effect {
                    DeleteEffect::Arm(confirm) => {
                        // The first `d` only names the entry; the footer
                        // asks for the second one.
                        self.status = None;
                        self.delete_confirm = Some(confirm);
                    }
                    DeleteEffect::Delete(id) => {
                        // The confirmation has been answered: drop it before
                        // the call, so a failed delete does not leave the UI
                        // waiting for a third `d`.
                        self.delete_confirm = None;
                        self.delete_entry(id);
                    }
                    DeleteEffect::Nothing => {
                        self.status = Some("nothing to delete".into());
                    }
                }
            }

            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.snapshot.entries.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.project_cursor = self.project_cursor.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l')
                if self.project_cursor + 1 < active_projects(&self.snapshot).len() =>
            {
                self.project_cursor += 1;
            }

            _ => {}
        }
    }
}

// --- the service thread ---

/// What the TUI asks the service thread to do.
///
/// Every bus call goes through the thread. The UI must never wait for the
/// service: a service that accepts the connection and then goes quiet would
/// freeze the TUI inside its refresh, and the user could not even quit
/// (issue #43).
#[derive(Debug)]
enum Request {
    /// The service announced a change (#103): refresh now instead of at
    /// the fallback tick. Sent by the signal thread, never by the UI.
    Refresh,
    /// Record one of the quick-add buckets, ending now.
    QuickAdd { project: String, ms: i64 },
    /// Record a duration ending now.
    AddDuration { project: String, ms: i64 },
    /// Record a hand-entered interval from the manual-entry form (#106).
    AddEntry {
        project: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    },
    /// Create a project (#106).
    AddProject { name: String },
    /// Export a period to `file` in the working directory (#106).
    Export {
        scope: timetrack_core::ExportScope,
        anchor_ms: i64,
        file: String,
    },
    /// Undo the last quick add.
    UndoQuickAdd,
    /// Delete an entry.
    DeleteEntry { id: String },
}

/// What the service thread reports back.
#[derive(Debug)]
enum Reply {
    /// A fresh snapshot of the service's state.
    Snapshot(Box<Snapshot>),
    /// A quick add worked: here is the entry it made.
    QuickAdded(Box<EntryView>),
    /// A duration was recorded: here is the entry it made.
    DurationAdded(Box<EntryView>),
    /// A hand-entered entry was recorded (#106).
    EntryAdded(Box<EntryView>),
    /// A project was created (#106).
    ProjectAdded(Box<ProjectView>),
    /// An export landed in `file` with `rows` entries (#106).
    Exported { file: String, rows: usize },
    /// An export itself worked but its file did not (#106): kept apart
    /// from `ActionFailed` because the service is blameless here.
    ExportFailed(String),
    /// A quick add was undone.
    Undone { id: String },
    /// An entry was deleted.
    Deleted { id: String },
    /// There is no quick add to undo.
    NothingToUndo,
    /// A refresh failed: the service is not answering.
    RefreshFailed(ClientError),
    /// An action failed.
    ActionFailed(ClientError),
}

/// Spawn the thread that talks to the service.
///
/// The UI thread never waits for the bus, so a wedged service costs a
/// message in the footer instead of a frozen screen: the TUI keeps drawing
/// and `q` keeps quitting while a call is outstanding (issue #43). The
/// thread ends when the UI drops its end of the channel.
fn spawn_service_thread(client: Client) -> (Receiver<Reply>, Sender<Request>) {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    // The `Changed` listener (#103): a clone of the same connection on
    // its own thread, forwarding each announcement as a refresh request.
    // Any failure is silence — the fallback tick still covers a
    // signalless service.
    {
        let client = client.clone();
        let requests = request_tx.clone();
        std::thread::spawn(move || {
            async_io::block_on(async move {
                let Ok(mut stream) = client.changed_stream().await else {
                    return;
                };
                while stream.next().await.is_some() {
                    if requests.send(Request::Refresh).is_err() {
                        return;
                    }
                }
            });
        });
    }
    std::thread::spawn(move || {
        // zbus uses the async-io backend (see the workspace Cargo.toml), so
        // the bus is driven with `async_io::block_on` on this thread, exactly
        // as the one-shot commands drive it.
        async_io::block_on(service_loop(client, request_rx, reply_tx));
    });
    (reply_rx, request_tx)
}

/// The service thread's loop: answer the UI's requests, and refresh on a
/// tick.
///
/// The requests are answered one at a time, in the order they were asked
/// for, so two quick adds in a row are two entries and an undo that follows
/// a quick add undoes that one.
async fn service_loop(client: Client, requests: Receiver<Request>, replies: Sender<Reply>) {
    // The entry the last quick add made, so `u` undoes exactly that one --
    // including one that has not been drawn yet (REQUIREMENTS §6).
    let mut last_quick_add_id: Option<String> = None;
    // The last snapshot, for the undo fallback when this thread never made
    // a quick add itself.
    let mut last_snapshot = Snapshot::default();
    // When the last refresh happened. `None` means "not yet", so the first
    // one is due at once and the screen is never blank for a whole tick.
    let mut last_refresh: Option<Instant> = None;
    loop {
        // Wait for a request, but never longer than until the next refresh
        // is due. zbus reads the socket on its own executor thread, so
        // waiting here does not stall the bus.
        let wait = match last_refresh {
            Some(at) => TICK.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        };
        let request = match requests.recv_timeout(wait) {
            Ok(request) => Some(request),
            // Nothing waiting: it is time to refresh.
            Err(RecvTimeoutError::Timeout) => None,
            // The UI is gone; nothing left to do.
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // A request of any kind — an action the user asked for, or a
        // `Changed` announcement forwarded by the signal thread — is
        // followed by a refresh below.
        let acted = request.is_some();
        match request {
            Some(Request::Refresh) => {}
            Some(Request::QuickAdd { project, ms }) => match client.quick_add(&project, ms).await {
                Ok(entry) => {
                    last_quick_add_id = Some(entry.id.clone());
                    let _ = replies.send(Reply::QuickAdded(Box::new(entry)));
                }
                Err(e) => {
                    let _ = replies.send(Reply::ActionFailed(e));
                }
            },
            Some(Request::AddDuration { project, ms }) => {
                match client.add_duration(&project, "", ms).await {
                    Ok(entry) => {
                        let _ = replies.send(Reply::DurationAdded(Box::new(entry)));
                    }
                    Err(e) => {
                        let _ = replies.send(Reply::ActionFailed(e));
                    }
                }
            }
            Some(Request::AddEntry {
                project,
                description,
                started_at,
                ended_at,
            }) => match client
                .add(&project, &description, started_at, ended_at)
                .await
            {
                Ok(entry) => {
                    let _ = replies.send(Reply::EntryAdded(Box::new(entry)));
                }
                Err(e) => {
                    let _ = replies.send(Reply::ActionFailed(e));
                }
            },
            Some(Request::AddProject { name }) => match client.add_project(&name).await {
                Ok(project) => {
                    let _ = replies.send(Reply::ProjectAdded(Box::new(project)));
                }
                Err(e) => {
                    let _ = replies.send(Reply::ActionFailed(e));
                }
            },
            Some(Request::Export {
                scope,
                anchor_ms,
                file,
            }) => match client.export_csv_at(scope.as_str(), anchor_ms).await {
                Ok(csv) => {
                    // Rows are the lines under the header.
                    let rows = csv.lines().count().saturating_sub(1);
                    match std::fs::write(&file, &csv) {
                        Ok(()) => {
                            let _ = replies.send(Reply::Exported { file, rows });
                        }
                        Err(e) => {
                            let _ = replies
                                .send(Reply::ExportFailed(format!("could not write {file}: {e}")));
                        }
                    }
                }
                Err(e) => {
                    let _ = replies.send(Reply::ActionFailed(e));
                }
            },
            Some(Request::UndoQuickAdd) => {
                // The target is resolved here, not by the UI: a quick add
                // that has not landed yet is still the one the user means to
                // undo, and the UI cannot know its id.
                match undo_target(last_quick_add_id.as_deref(), &last_snapshot) {
                    Some(id) => match client.undo_quick_add(&id).await {
                        Ok(()) => {
                            let _ = replies.send(Reply::Undone { id });
                        }
                        Err(e) => {
                            let _ = replies.send(Reply::ActionFailed(e));
                        }
                    },
                    None => {
                        let _ = replies.send(Reply::NothingToUndo);
                    }
                }
            }
            Some(Request::DeleteEntry { id }) => match client.delete_entry(&id).await {
                Ok(()) => {
                    let _ = replies.send(Reply::Deleted { id });
                }
                Err(e) => {
                    let _ = replies.send(Reply::ActionFailed(e));
                }
            },
            None => {}
        }
        // An action changes the data, so refresh straight away: the screen
        // must show what the action did, not the state before it. Otherwise
        // refresh on the tick, so a service that has gone quiet shows up as
        // a failure instead of a frozen screen.
        if acted || last_refresh.is_none_or(|at| at.elapsed() >= TICK) {
            match client.snapshot().await {
                Ok(s) => {
                    last_snapshot = s.clone();
                    let _ = replies.send(Reply::Snapshot(Box::new(s)));
                }
                Err(e) => {
                    let _ = replies.send(Reply::RefreshFailed(e));
                }
            }
            last_refresh = Some(Instant::now());
        }
    }
}

/// The entry an undo should remove.
///
/// The quick add this session made wins over the newest one in the
/// snapshot: a quick add that has not landed yet is still the one the user
/// means to undo, and the snapshot cannot know its id.
fn undo_target(last_quick_add_id: Option<&str>, snap: &Snapshot) -> Option<String> {
    last_quick_add_id
        .map(|id| id.to_string())
        .or_else(|| last_quick_add(snap))
}

/// Projects offered for new entries: archived ones are hidden, because
/// archiving is about not choosing them, not about erasing their history
/// (REQUIREMENTS §14).
fn active_projects(snap: &Snapshot) -> Vec<&ProjectView> {
    snap.projects.iter().filter(|p| !p.archived).collect()
}

/// The project at `cursor` in the picker's order.
///
/// A free function over the snapshot rather than a method on `App`, so the
/// archived-filtering rule is testable without a live service connection.
fn current_project(snap: &Snapshot, cursor: usize) -> Option<&ProjectView> {
    active_projects(snap).get(cursor).copied()
}

/// The name of a project, or its id when the snapshot does not have it.
///
/// Sanitized for display (#101): this one resolver feeds the entry rows and
/// the action messages, so stored control characters never reach a cell.
fn project_name(snap: &Snapshot, id: &str) -> String {
    snap.projects
        .iter()
        .find(|p| p.id == id)
        .map(|p| timetrack_core::sanitize_display(&p.name))
        .unwrap_or_else(|| id.to_string())
}

/// The newest quick-add in the list.
///
/// The fallback for an undo when this session has not made a quick add
/// itself. Entries arrive newest first, so the first match is the newest.
fn last_quick_add(snap: &Snapshot) -> Option<String> {
    snap.entries
        .iter()
        .find(|e| e.source == timetrack_proto::EntrySource::QuickAdd)
        .map(|e| e.id.clone())
}

/// The delete confirmation for the entry at `selected`, if there is one.
///
/// A free function over the snapshot, like the other rules above, so the
/// wording is testable without a live service connection. The summary names
/// the project and the duration in the tray's words (`Delete 30m from
/// Work`), so the three clients cannot drift apart on what a delete would
/// take.
fn delete_offer(snap: &Snapshot, selected: usize) -> Option<DeleteConfirm> {
    let picked = snap.entries.get(selected)?;
    let project = snap
        .projects
        .iter()
        .find(|p| p.id == picked.project_id)
        .map(|p| timetrack_core::sanitize_display(&p.name))
        .unwrap_or_else(|| picked.project_id.clone());
    Some(DeleteConfirm {
        id: picked.id.clone(),
        summary: format!(
            "Delete {} from {}? d to confirm, esc to cancel",
            timetrack_core::format_compact(picked.duration_ms()),
            project,
        ),
    })
}

/// What the `d` key does.
#[derive(Debug, PartialEq, Eq)]
enum DeleteEffect {
    /// There is no entry to delete.
    Nothing,
    /// Show this confirmation and wait for a second `d`.
    Arm(DeleteConfirm),
    /// Delete this entry.
    Delete(String),
}

/// The effect of the `d` key, given what is already pending.
///
/// Pure, so the two-key rule is testable without a service connection: the
/// first `d` only names the entry, and only an armed confirmation can turn
/// into a delete.
fn delete_effect(
    pending: Option<&DeleteConfirm>,
    snap: &Snapshot,
    selected: usize,
) -> DeleteEffect {
    match pending {
        Some(confirm) => DeleteEffect::Delete(confirm.id.clone()),
        None => match delete_offer(snap, selected) {
            Some(confirm) => DeleteEffect::Arm(confirm),
            None => DeleteEffect::Nothing,
        },
    }
}

/// What a key press means while a delete confirmation is pending.
#[derive(Debug, PartialEq, Eq)]
enum PendingAnswer {
    /// `d`: delete the armed entry.
    Confirm,
    /// `Esc`, or any other key: disarm, change nothing.
    Disarm,
    /// `q` (or Ctrl-C): leave.
    Quit,
}

/// The answer a key press gives to a pending delete confirmation.
///
/// Free of `App`, so the rule is testable without a service connection: only
/// `d` confirms, and everything else but `q` disarms without deleting.
fn pending_answer(code: KeyCode, ctrl: bool) -> PendingAnswer {
    match code {
        KeyCode::Char('d') => PendingAnswer::Confirm,
        KeyCode::Char('c') if ctrl => PendingAnswer::Quit,
        KeyCode::Char('q') => PendingAnswer::Quit,
        _ => PendingAnswer::Disarm,
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    // An open prompt takes the footer's place and grows to its fields
    // plus border and hint/error line; the week total stays topmost.
    let footer_len = match &app.prompt {
        Some(p) => p.fields.len() as u16 + 3,
        None => 1,
    };
    let chunks = Layout::vertical([
        Constraint::Length(5), // the week total -- the topmost element
        Constraint::Length(3), // quick-add row
        Constraint::Length(6), // this week, per project
        Constraint::Min(3),    // recent entries
        Constraint::Length(footer_len),
    ])
    .split(area);

    draw_week(f, chunks[0], app);
    draw_quick_add(f, chunks[1], app);
    draw_projects(f, chunks[2], app);
    draw_entries(f, chunks[3], app);
    match &app.prompt {
        Some(prompt) => draw_prompt(f, chunks[4], prompt),
        None => draw_footer(f, chunks[4], app),
    }
}

/// The open prompt (#106): one line per field, the active one marked and
/// tailed by a cursor bar, then the error that kept it open or the keys
/// that move through it. Field values are sanitized for display (#101):
/// they are typed text on its way to being stored text.
fn draw_prompt(f: &mut ratatui::Frame, area: Rect, prompt: &Prompt) {
    let mut lines: Vec<Line> = prompt
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let active = i == prompt.active;
            let mut spans = vec![
                Span::raw(if active { "▶ " } else { "  " }),
                Span::styled(
                    format!("{}: ", field.label),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(timetrack_core::sanitize_display(&field.value)),
            ];
            if active {
                spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
            }
            Line::from(spans)
        })
        .collect();
    match &prompt.error {
        Some(e) => lines.push(Line::from(Span::styled(
            timetrack_core::sanitize_display(e),
            Style::default().fg(Color::Red),
        ))),
        None => lines.push(Line::from(Span::styled(
            "tab next field · enter done · esc cancel",
            Style::default().fg(Color::DarkGray),
        ))),
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(timetrack_core::sanitize_display(&prompt.title)),
        ),
        area,
    );
}

/// The week total, in the largest type the terminal can give us.
///
/// Nothing is drawn above this: the number is the answer the app exists to
/// give, so it gets the first line of the screen.
fn draw_week(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let week = &app.snapshot.week;
    let mut lines = vec![Line::from(Span::styled(
        timetrack_core::format_duration(week.total_ms).to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];

    let detail = if week.entry_count == 1 {
        "this week · 1 entry".to_string()
    } else {
        format!("this week · {} entries", week.entry_count)
    };
    lines.push(Line::from(Span::styled(
        detail,
        Style::default().fg(Color::DarkGray),
    )));

    // A day over 24h is legal but worth saying out loud (REQUIREMENTS §4).
    if !app.snapshot.over_24h_days.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "⚠ {} day(s) total more than 24h",
                app.snapshot.over_24h_days.len()
            ),
            Style::default().fg(Color::Yellow),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .alignment(ratatui::layout::Alignment::Center),
        area,
    );
}

/// The 5/15/30/60 row, with the target project named.
fn draw_quick_add(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        "  add:  ",
        Style::default().fg(Color::DarkGray),
    )];
    for (i, ms) in QUICK_ADD_MS.iter().enumerate() {
        spans.push(Span::styled(
            format!("[{}] {}m  ", i + 1, ms / 60_000),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let project = app
        .current_project()
        .map(|p| timetrack_core::sanitize_display(&p.name))
        .unwrap_or_else(|| "no project".into());
    spans.push(Span::raw("  →  "));
    spans.push(Span::styled(project, Style::default().fg(Color::Yellow)));

    f.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

/// This week's time per project, longest first.
fn draw_projects(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let active = active_projects(&app.snapshot);
    let items: Vec<ListItem> = if active.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no projects yet",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        // Rank by this week's time so the bar order is meaningful; fall back
        // to the project list order for projects with no time yet.
        // Display names are sanitized copies (#101), so rows carry the id
        // for identity: the selected check compares ids, never display text.
        let mut rows: Vec<(&str, String, i64)> = active
            .iter()
            .map(|p| {
                (
                    p.id.as_str(),
                    timetrack_core::sanitize_display(&p.name),
                    app.snapshot
                        .week
                        .per_project
                        .get(&p.id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        rows.into_iter()
            .map(|(id, name, ms)| {
                let selected = app.current_project().map(|p| p.id == id).unwrap_or(false);
                ListItem::new(Line::from(vec![
                    Span::raw(if selected { "▶ " } else { "  " }),
                    Span::raw(format!("{name:<28}")),
                    Span::styled(
                        format!("{:>10}", timetrack_core::format_duration(ms)),
                        Style::default().fg(Color::Cyan),
                    ),
                ]))
            })
            .collect()
    };

    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("This week (h/l to change project)"),
        ),
        area,
    );
}

/// Recent entries. Everything is closed, so there is no running marker.
fn draw_entries(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let project_name = |id: &str| -> String { project_name(&app.snapshot, id) };

    // The entry a pending delete would take, so the row that is about to go
    // is the one the eye is on.
    let armed = app.delete_confirm.as_ref().map(|c| c.id.as_str());

    let items: Vec<ListItem> = if app.snapshot.entries.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  nothing tracked yet — press 1 to add 5 minutes",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.snapshot
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let style = if armed == Some(e.id.as_str()) {
                    Style::default()
                        .bg(Color::Red)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else if i == app.selected {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                // A quick-add is marked so `u` and the user can tell undoable
                // entries from hand-entered ones at a glance.
                let marker = if e.source == timetrack_proto::EntrySource::QuickAdd {
                    "+"
                } else {
                    " "
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(
                        format!("{:>10}", timetrack_core::format_duration(e.duration_ms())),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(format!(
                        "  {:<22}",
                        truncate(&project_name(&e.project_id), 22)
                    )),
                    Span::raw(timetrack_core::sanitize_display(e.label())),
                ]))
                .style(style)
            })
            .collect()
    };

    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Recent entries (+ = quick add)"),
        ),
        area,
    );
}

/// The footer: the last action's message, or the key hints.
///
/// A pending delete confirmation outranks the message: it is the question
/// the user has to answer before anything else happens.
/// The key hints, shown in the footer when there is nothing else to say.
///
/// `u` undoes a *quick add*, and `a` is not one: it records a plain
/// 30-minute entry, the same one the GUI's dialog would make with its end
/// prefilled at now. Saying so here is the difference between a rule the
/// user has to discover by pressing `u` and being refused, and a rule they
/// can read.
fn footer_hint() -> &'static str {
    "1-4 quick add  a 30m  e new entry  p new project  7/8/9 export  u undo quick add  d delete  j/k move  h/l project  q quit"
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // Sanitized (#101): the status can carry a service refusal embedding
    // stored text, and the footer is one line of terminal cells.
    let (footer, style) = match &app.delete_confirm {
        Some(confirm) => (
            confirm.summary.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        None => match &app.status {
            Some(s) => (s.clone(), Style::default().fg(Color::DarkGray)),
            None => (
                footer_hint().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        },
    };
    f.render_widget(
        Paragraph::new(timetrack_core::sanitize_display(&footer)).style(style),
        area,
    );
}

/// Shorten to `max` characters, so a long project name cannot push the rest
/// of the row off screen.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub async fn run() -> Result<()> {
    let client = Client::wait_for_service(std::time::Duration::from_secs(5)).await?;

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = event_loop(&mut terminal, client);

    // Always restore the terminal, even if the loop failed, or the user is
    // left with a broken shell.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

/// The TUI's event loop: draw, take what the service thread reported, read
/// a key.
///
/// Nothing here waits for the bus. The calls happen on the service thread,
/// so a wedged service cannot stop the loop: the screen keeps being drawn
/// and `q` keeps quitting while a call is outstanding (issue #43).
fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, client: Client) -> Result<()> {
    let (replies, requests) = spawn_service_thread(client);
    let mut app = App::new(requests);

    while !app.should_quit {
        terminal.draw(|f| draw(f, &app))?;

        // Take whatever the service thread has reported since last time.
        while let Ok(reply) = replies.try_recv() {
            app.apply(reply);
        }

        if event::poll(POLL)? {
            if let Event::Key(key) = event::read()? {
                // Windows/GTK emit both press and release; acting on both would
                // double every command.
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timetrack_proto::{EntrySource, EntryView};

    #[test]
    fn the_footer_says_what_undo_removes() {
        // `u` removes a quick add, not the 30 minutes `a` records. The
        // footer is where the rules live when there is no key to press.
        let hint = footer_hint();
        assert!(
            hint.contains("u undo quick add"),
            "the footer says what undo removes: {hint}"
        );
        assert!(hint.contains("a 30m"), "the footer lists `a`: {hint}");
    }

    /// An entry that started `start` ms ago. The service sends entries newest
    /// first, so a test that depends on that order must vary the start time.
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

    fn project(id: &str, name: &str, archived: bool) -> ProjectView {
        ProjectView {
            id: id.into(),
            name: name.into(),
            colour: None,
            archived,
        }
    }

    /// An entry that ran for `minutes` minutes.
    fn entry_minutes(id: &str, source: EntrySource, start: i64, minutes: i64) -> EntryView {
        EntryView {
            ended_at: start + minutes * 60_000,
            ..entry(id, source, start)
        }
    }

    #[test]
    fn quick_add_buckets_are_5_15_30_60() {
        // The Home row and the number keys are hardcoded to these, so a change
        // to the core constant must break this test rather than the UI.
        let mins: Vec<i64> = QUICK_ADD_MS.iter().map(|m| m / 60_000).collect();
        assert_eq!(mins, [5, 15, 30, 60]);
    }

    #[test]
    fn archived_projects_are_hidden_from_the_picker() {
        // Archiving changes what can be chosen, not what exists: the history
        // stays in totals, but offering it in the picker would defeat it.
        let snap = Snapshot {
            projects: vec![project("p1", "Work", false), project("p2", "Old", true)],
            ..Default::default()
        };
        let names: Vec<&str> = active_projects(&snap)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Work"]);
    }

    #[test]
    fn the_picker_never_lands_on_an_archived_project() {
        // `current_project` used to index the unfiltered list, so with an
        // archived project anywhere but the end every cursor past it picked
        // the wrong project -- or an archived one -- for quick-adds.
        let snap = Snapshot {
            projects: vec![
                project("p1", "Work", false),
                project("p2", "Old", true),
                project("p3", "Personal", false),
            ],
            ..Default::default()
        };
        assert_eq!(current_project(&snap, 0).map(|p| p.id.as_str()), Some("p1"));
        assert_eq!(current_project(&snap, 1).map(|p| p.id.as_str()), Some("p3"));
        assert_eq!(current_project(&snap, 2), None);
    }

    #[test]
    fn last_quick_add_prefers_the_one_we_just_made() {
        let snap = Snapshot {
            // Newest first, as the service sends them.
            entries: vec![
                entry("e3", EntrySource::QuickAdd, 2_000),
                entry("e2", EntrySource::Manual, 1_000),
                entry("e1", EntrySource::QuickAdd, 0),
            ],
            ..Default::default()
        };
        // Entries are newest first, so e3 is the most recent quick add.
        assert_eq!(last_quick_add(&snap).as_deref(), Some("e3"));
    }

    #[test]
    fn there_is_nothing_to_undo_without_a_quick_add() {
        let snap = Snapshot {
            entries: vec![entry("e1", EntrySource::Manual, 0)],
            ..Default::default()
        };
        assert_eq!(last_quick_add(&snap), None);
    }

    #[test]
    fn the_delete_confirmation_names_the_project_and_the_duration() {
        // The confirmation has to say what would go, in the tray's words:
        // the project and the duration, never a bare id.
        let snap = Snapshot {
            projects: vec![project("p1", "Work", false)],
            entries: vec![entry_minutes("e1", EntrySource::Manual, 0, 30)],
            ..Default::default()
        };
        let offer = delete_offer(&snap, 0).expect("e1 is selected");
        assert_eq!(offer.id, "e1");
        assert_eq!(
            offer.summary,
            "Delete 30m from Work? d to confirm, esc to cancel"
        );
    }

    #[test]
    fn stored_names_are_sanitized_before_they_reach_a_cell() {
        // #101: a legacy store's project name can hold controls the current
        // rules refuse. The resolver every row uses, and the confirmation
        // built on a raw lookup, both hand the terminal a clean copy.
        let snap = Snapshot {
            projects: vec![project("p1", "Wo\nrk\u{1b}[31m", false)],
            entries: vec![entry_minutes("e1", EntrySource::Manual, 0, 30)],
            ..Default::default()
        };
        assert_eq!(project_name(&snap, "p1"), "Wo rk [31m");
        let offer = delete_offer(&snap, 0).unwrap();
        assert!(
            !offer.summary.chars().any(|c| c.is_control()),
            "{}",
            offer.summary
        );
        assert!(offer.summary.contains("Wo rk [31m"), "{}", offer.summary);
    }

    #[test]
    fn there_is_no_confirmation_without_an_entry() {
        let snap = Snapshot {
            projects: vec![project("p1", "Work", false)],
            entries: vec![entry("e1", EntrySource::Manual, 0)],
            ..Default::default()
        };
        assert!(delete_offer(&snap, 99).is_none());
        assert!(delete_offer(&Snapshot::default(), 0).is_none());
    }

    #[test]
    fn the_first_d_only_arms_the_confirmation() {
        // The two-key rule the GUI and the tray already have: the first `d`
        // names the entry, the second removes it. An unarmed delete changes
        // nothing in the store.
        let snap = Snapshot {
            projects: vec![project("p1", "Work", false)],
            entries: vec![entry_minutes("e1", EntrySource::Manual, 0, 30)],
            ..Default::default()
        };
        match delete_effect(None, &snap, 0) {
            DeleteEffect::Arm(confirm) => assert_eq!(confirm.id, "e1"),
            other => panic!("the first d must only arm the confirmation: {other:?}"),
        }
        // Nothing to delete: no confirmation, no delete.
        assert_eq!(
            delete_effect(None, &Snapshot::default(), 0),
            DeleteEffect::Nothing
        );
    }

    #[test]
    fn the_second_d_deletes_the_armed_entry() {
        let snap = Snapshot {
            projects: vec![project("p1", "Work", false)],
            entries: vec![entry_minutes("e1", EntrySource::Manual, 0, 30)],
            ..Default::default()
        };
        let armed = delete_offer(&snap, 0).expect("e1 is selected");
        assert_eq!(
            delete_effect(Some(&armed), &snap, 0),
            DeleteEffect::Delete("e1".to_string())
        );
    }

    #[test]
    fn only_a_second_d_deletes_and_any_other_key_disarms() {
        // While the confirmation is pending, only `d` answers it; `Esc` and
        // every other key disarm it without deleting, and `q` still leaves.
        assert_eq!(
            pending_answer(KeyCode::Char('d'), false),
            PendingAnswer::Confirm
        );
        assert_eq!(pending_answer(KeyCode::Esc, false), PendingAnswer::Disarm);
        assert_eq!(
            pending_answer(KeyCode::Char('j'), false),
            PendingAnswer::Disarm
        );
        assert_eq!(
            pending_answer(KeyCode::Char('1'), false),
            PendingAnswer::Disarm
        );
        assert_eq!(
            pending_answer(KeyCode::Char('q'), false),
            PendingAnswer::Quit
        );
        assert_eq!(
            pending_answer(KeyCode::Char('c'), true),
            PendingAnswer::Quit
        );
    }

    // --- the wedged service (issue #43) ---

    /// A key press, as the event loop would hand it over.
    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// An app whose commands go nowhere, and the channel they would go to.
    fn app() -> (App, Receiver<Request>) {
        let (requests, receiver) = std::sync::mpsc::channel();
        (App::new(requests), receiver)
    }

    #[test]
    fn a_wedged_service_is_reported_in_the_footer_and_the_ui_keeps_running() {
        // A service that accepts the connection and then never answers must
        // not take the TUI down: the failure lands in the footer as a
        // notice, and the UI carries on (issue #43).
        let (mut app, _requests) = app();
        app.apply(Reply::RefreshFailed(ClientError::Transport(
            "the service did not answer in time".into(),
        )));
        let status = app.status.as_deref().expect("a status message");
        assert!(
            status.starts_with("service unavailable"),
            "the footer must say the service is unavailable: {status}"
        );
        assert!(!app.should_quit, "a wedged service must not quit the TUI");
    }

    #[test]
    fn the_unavailable_notice_clears_when_the_service_answers_again() {
        // Recovery is the other half of the notice: once a snapshot lands,
        // the "service unavailable" line goes away, and nothing else does.
        let (mut app, _requests) = app();
        app.apply(Reply::RefreshFailed(ClientError::Transport(
            "the service did not answer in time".into(),
        )));
        app.apply(Reply::Snapshot(Box::default()));
        assert!(
            app.status.is_none(),
            "the notice clears on recovery, got {:?}",
            app.status
        );

        // An action's own message survives the next poll.
        app.apply(Reply::Undone { id: "e1".into() });
        app.apply(Reply::Snapshot(Box::default()));
        assert_eq!(app.status.as_deref(), Some("undid quick add e1"));
    }

    #[test]
    fn a_failed_action_says_what_went_wrong_and_keeps_the_ui_alive() {
        let (mut app, _requests) = app();
        app.apply(Reply::ActionFailed(ClientError::Transport(
            "the service did not answer in time".into(),
        )));
        assert_eq!(
            app.status.as_deref(),
            Some("the service did not answer in time")
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn a_key_press_asks_the_service_thread_rather_than_calling_the_bus() {
        // The UI never calls the service itself: it hands the request over
        // and carries on. That is what keeps the event loop alive while a
        // call is outstanding (issue #43).
        let (mut app, requests) = app();
        app.snapshot = Snapshot {
            projects: vec![project("p1", "Work", false)],
            ..Default::default()
        };
        app.on_key(key(KeyCode::Char('1')));
        match requests.try_recv().expect("a request") {
            Request::QuickAdd { project, ms } => {
                assert_eq!(project, "p1");
                assert_eq!(ms, QUICK_ADD_MS[0]);
            }
            other => panic!("expected a quick add, got {other:?}"),
        }
    }

    #[test]
    fn manual_entry_resolves_in_the_services_zone() {
        // #106: the form speaks the shared spellings, in the zone the
        // service named (#102) — Rome wall clocks on a known day.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let now = timetrack_core::parse_anchor_day("2026-10-08", "anchor", &rome).unwrap();
        let got = resolve_manual_entry("09:00", "10:30", "review", now, &rome).unwrap();
        assert_eq!(got.ended_at - got.started_at, 90 * 60_000);
        assert_eq!(
            timetrack_core::format_local_hm(got.started_at, &rome),
            "09:00"
        );
        assert_eq!(got.description, "review");
        // Empty end means now; a relative start measures from it.
        let got = resolve_manual_entry("-45", "", "", now, &rome).unwrap();
        assert_eq!(got.ended_at, now);
        assert_eq!(got.ended_at - got.started_at, 45 * 60_000);
        // Backwards and unparseable are form errors, not service calls.
        assert!(resolve_manual_entry("10:30", "09:00", "", now, &rome).is_err());
        assert!(resolve_manual_entry("25:99", "", "", now, &rome).is_err());
        assert!(resolve_manual_entry("", "", "", now, &rome).is_err());
    }

    #[test]
    fn export_resolution_names_the_periods_file() {
        // #106: the anchor day picks the period; the file name comes
        // from the core's one table, like the GUI's.
        let utc = timetrack_core::Tz::utc();
        let (anchor, file) =
            resolve_export("2026-10-08", timetrack_core::ExportScope::Week, &utc).unwrap();
        assert_eq!(file, "timetrack-export-week-2026-W41.csv");
        assert_eq!(
            anchor,
            timetrack_core::parse_anchor_day("2026-10-08", "anchor day", &utc).unwrap()
        );
        assert!(resolve_export("2026-02-31", timetrack_core::ExportScope::Month, &utc).is_err());
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn app_with_project() -> (App, Receiver<Request>) {
        let (mut app, requests) = app();
        app.snapshot = Snapshot {
            projects: vec![project("p1", "Work", false)],
            tz: "UTC".into(),
            ..Default::default()
        };
        (app, requests)
    }

    #[test]
    fn p_creates_a_project_and_the_picker_follows_it() {
        // #106: `p` + a name makes the project through the service
        // thread, and the next snapshot lands the cursor on it.
        let (mut app, requests) = app_with_project();
        app.on_key(key(KeyCode::Char('p')));
        type_text(&mut app, "Ops");
        app.on_key(key(KeyCode::Enter));
        match requests.try_recv().expect("a request") {
            Request::AddProject { name } => assert_eq!(name, "Ops"),
            other => panic!("expected AddProject, got {other:?}"),
        }
        app.apply(Reply::ProjectAdded(Box::new(project("p2", "Ops", false))));
        app.apply(Reply::Snapshot(Box::new(Snapshot {
            projects: vec![project("p1", "Work", false), project("p2", "Ops", false)],
            ..Default::default()
        })));
        assert_eq!(app.current_project().map(|p| p.id.as_str()), Some("p2"));
        assert_eq!(app.status.as_deref(), Some("created project Ops"));
    }

    #[test]
    fn e_records_a_manual_entry_and_commands_do_not_fire_while_typing() {
        // The prompt owns the keyboard: `q` inside it is a letter, and
        // Enter walks the fields before submitting exactly one entry.
        let (mut app, requests) = app_with_project();
        app.on_key(key(KeyCode::Char('e')));
        assert!(app.prompt.is_some());
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit, "q is text inside a prompt");
        // The `q` landed in the start field ("-30q"); remove it again.
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Enter)); // -> end (empty = now)
        app.on_key(key(KeyCode::Enter)); // -> description
        type_text(&mut app, "sync");
        app.on_key(key(KeyCode::Enter)); // submit
        match requests.try_recv().expect("a request") {
            Request::AddEntry {
                project,
                description,
                started_at,
                ended_at,
            } => {
                assert_eq!(project, "p1");
                assert_eq!(description, "sync");
                assert_eq!(ended_at - started_at, 30 * 60_000);
            }
            other => panic!("expected AddEntry, got {other:?}"),
        }
        assert!(app.prompt.is_none(), "a submitted form closes");
    }

    #[test]
    fn a_bad_time_keeps_the_form_open_with_its_error() {
        let (mut app, requests) = app_with_project();
        app.on_key(key(KeyCode::Char('e')));
        type_text(&mut app, "x");
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        let prompt = app.prompt.as_ref().expect("the form stays open");
        assert!(prompt.error.is_some(), "with the parse error shown");
        assert!(requests.try_recv().is_err(), "nothing was sent");
        app.on_key(key(KeyCode::Esc));
        assert!(app.prompt.is_none(), "esc leaves");
    }

    #[test]
    fn seven_asks_for_the_week_and_nine_exports_everything_now() {
        let (mut app, requests) = app_with_project();
        app.on_key(key(KeyCode::Char('9')));
        match requests.try_recv().expect("a request") {
            Request::Export { scope, file, .. } => {
                assert_eq!(scope, timetrack_core::ExportScope::All);
                assert_eq!(file, "timetrack-export-all.csv");
            }
            other => panic!("expected Export, got {other:?}"),
        }
        app.on_key(key(KeyCode::Char('7')));
        assert!(app.prompt.is_some(), "the week needs its anchor day");
        app.on_key(key(KeyCode::Enter)); // accept today's prefill
        match requests.try_recv().expect("a request") {
            Request::Export { scope, file, .. } => {
                assert_eq!(scope, timetrack_core::ExportScope::Week);
                assert!(file.starts_with("timetrack-export-week-"), "{file}");
            }
            other => panic!("expected Export, got {other:?}"),
        }
    }

    #[test]
    fn q_quits_even_while_a_call_is_outstanding() {
        // The whole point of the service thread: the UI thread never waits
        // for the bus, so the quit key always works.
        let (mut app, _requests) = app();
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn undo_targets_the_quick_add_this_session_made() {
        // The target is resolved by the service thread, so an undo typed
        // right after a quick add undoes *that* one -- the snapshot cannot
        // know the id of a quick add that has not landed yet.
        let snap = Snapshot {
            entries: vec![entry("e2", EntrySource::QuickAdd, 0)],
            ..Default::default()
        };
        assert_eq!(undo_target(Some("e9"), &snap).as_deref(), Some("e9"));
        assert_eq!(undo_target(None, &snap).as_deref(), Some("e2"));
        assert_eq!(undo_target(None, &Snapshot::default()), None);
    }

    #[test]
    fn selection_clamps_to_a_shrunk_list() {
        // Simulates the guard in App::apply: deleting the last entry while
        // the cursor sits on it must not leave the index out of bounds.
        let mut selected: usize = 5;
        let len: usize = 2;
        if selected >= len {
            selected = len.saturating_sub(1);
        }
        assert_eq!(selected, 1);
    }

    #[test]
    fn empty_list_selection_is_zero() {
        let selected: usize = 3;
        let len: usize = 0;
        let clamped = if selected >= len {
            len.saturating_sub(1)
        } else {
            selected
        };
        assert_eq!(clamped, 0);
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("Work", 22), "Work");
    }

    #[test]
    fn truncate_shortens_and_marks_the_cut() {
        let long = "a".repeat(40);
        let out = truncate(&long, 22);
        assert_eq!(out.chars().count(), 22);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // A multi-byte name must not be cut mid-character.
        let out = truncate(&"caffè".repeat(10), 22);
        assert_eq!(out.chars().count(), 22);
    }
}
