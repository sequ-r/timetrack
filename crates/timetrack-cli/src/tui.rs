/* cli/tui.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The ratatui front end.
//!
//! The service is the source of truth, so the TUI polls it rather than
//! holding state. Nothing is running any more, so there is no local ticking
//! to interpolate: every number on screen came from the last poll, and
//! repainting at 1Hz is enough.
//!
//! # Layout mirrors the GUI's Home tab
//!
//! The week total is the largest thing on screen and nothing is above it
//! (REQUIREMENTS §7), then the quick-add row, then this week's projects,
//! then the recent entries. The GUI and the CLI showing the same hierarchy in
//! the same order is deliberate.

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
use std::time::{Duration, Instant};
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::{Client, ClientError, ProjectView, Snapshot};

/// How often to refresh from the service.
const TICK: Duration = Duration::from_millis(1000);

/// How often to check for a keypress while idle.
const POLL: Duration = Duration::from_millis(100);

struct App {
    client: Client,
    snapshot: Snapshot,
    selected: usize,
    /// Index into the *active* projects for the project quick-adds go to.
    project_cursor: usize,
    /// The entry the last quick-add created, so `u` undoes exactly that one.
    ///
    /// Tracked here rather than re-derived from the list, because the list is
    /// ordered by start time and two quick-adds can share a timestamp; the
    /// id we were handed is the only unambiguous answer (REQUIREMENTS §6).
    last_quick_add_id: Option<String>,
    /// A transient message shown in the footer.
    status: Option<String>,
    should_quit: bool,
}

impl App {
    async fn refresh(&mut self) {
        match self.client.snapshot().await {
            Ok(s) => {
                self.snapshot = s;
                // The lists may have shrunk under the cursor.
                if self.selected >= self.snapshot.entries.len() {
                    self.selected = self.snapshot.entries.len().saturating_sub(1);
                }
                let project_count = active_projects(&self.snapshot).len();
                if self.project_cursor >= project_count {
                    self.project_cursor = project_count.saturating_sub(1);
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
            Err(e) => self.status = Some(format!("service unavailable: {e}")),
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.snapshot
            .entries
            .get(self.selected)
            .map(|e| e.id.clone())
    }

    /// The project quick-add and the new-entry dialogs attribute time to.
    ///
    /// Indexes the *active* projects, not the raw list: archiving hides a
    /// project from the picker, so the cursor must not be able to land on one
    /// (or every index past it would attribute time to the wrong project).
    fn current_project(&self) -> Option<&ProjectView> {
        current_project(&self.snapshot, self.project_cursor)
    }

    /// Run one action, recording any failure in the footer rather than
    /// exiting: a transient bus error should not kill the UI.
    ///
    /// Takes a closure rather than a future so the call is not evaluated (and
    /// not sent to the bus) until it is actually awaited here. Returns whether
    /// the call succeeded, so the caller prints "undid ..." only when
    /// something was actually undone.
    async fn act<F, Fut>(&mut self, f: F) -> bool
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(), ClientError>>,
    {
        // Client is a cheap handle over the same connection, so clone it out
        // rather than borrow `self` across the call: the call needs `&mut self`
        // again afterwards to refresh and to record any error.
        let client = self.client.clone();
        let ok = match f(client).await {
            Ok(()) => true,
            Err(e) => {
                self.status = Some(e.to_string());
                false
            }
        };
        self.refresh().await;
        ok
    }

    async fn on_key(&mut self, key: event::KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('q') => self.should_quit = true,

            // --- quick add: 5/15/30/60 on the number row (REQUIREMENTS §6) ---
            KeyCode::Char('1') => self.quick_add(QUICK_ADD_MS[0]).await,
            KeyCode::Char('2') => self.quick_add(QUICK_ADD_MS[1]).await,
            KeyCode::Char('3') => self.quick_add(QUICK_ADD_MS[2]).await,
            KeyCode::Char('4') => self.quick_add(QUICK_ADD_MS[3]).await,

            // --- method 2: a duration ending now ---
            KeyCode::Char('a') => {
                let Some(project) = self.current_project().map(|p| p.id.clone()) else {
                    self.status = Some(
                        "no project yet -- create one with: timetrack project \"Work\"".into(),
                    );
                    return;
                };
                // Thirty minutes is the middle default and the least
                // destructive thing to add by accident.
                let id = project;
                let client = self.client.clone();
                let label = self
                    .current_project()
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                let res = client.add_duration(&id, "", 30 * 60_000).await;
                match res {
                    Ok(_) => self.status = Some(format!("added 30m to {label}")),
                    Err(e) => self.status = Some(e.to_string()),
                }
                self.refresh().await;
            }

            // --- undo: only ever the last quick add ---
            KeyCode::Char('u') => {
                let undo_id = self
                    .last_quick_add_id
                    .clone()
                    .or_else(|| last_quick_add(&self.snapshot));
                if let Some(id) = undo_id {
                    let for_call = id.clone();
                    let ok = self
                        .act(move |c| {
                            let id = for_call.clone();
                            async move { c.undo_quick_add(&id).await }
                        })
                        .await;
                    if ok {
                        self.status = Some(format!("undid quick add {id}"));
                    }
                } else {
                    self.status = Some("nothing to undo".into());
                }
            }

            // --- explicit delete ---
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_id() {
                    let for_call = id.clone();
                    let ok = self
                        .act(move |c| {
                            let id = for_call.clone();
                            async move { c.delete_entry(&id).await }
                        })
                        .await;
                    if ok {
                        self.status = Some(format!("deleted {id}"));
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

    /// Add one of the quick-add buckets to the selected project.
    async fn quick_add(&mut self, duration_ms: i64) {
        let Some(project) = self.current_project().map(|p| p.id.clone()) else {
            self.status =
                Some("no project yet -- create one with: timetrack project \"Work\"".into());
            return;
        };
        let name = self
            .current_project()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let client = self.client.clone();
        match client.quick_add(&project, duration_ms).await {
            Ok(e) => {
                self.last_quick_add_id = Some(e.id.clone());
                self.status = Some(format!(
                    "added {} to {name} (u to undo)",
                    timetrack_core::format_compact(e.duration_ms())
                ));
            }
            Err(err) => self.status = Some(err.to_string()),
        }
        self.refresh().await;
    }
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

/// The entry `u` would undo.
///
/// Prefers the id handed back by the most recent quick-add call, and falls
/// back to the newest quick-add in the list for a client that has just
/// started. Entries arrive newest first, so the first match is the newest.
fn last_quick_add(snap: &Snapshot) -> Option<String> {
    snap.entries
        .iter()
        .find(|e| e.source == timetrack_proto::EntrySource::QuickAdd)
        .map(|e| e.id.clone())
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(5), // the week total -- the topmost element
        Constraint::Length(3), // quick-add row
        Constraint::Length(6), // this week, per project
        Constraint::Min(3),    // recent entries
        Constraint::Length(1), // footer
    ])
    .split(area);

    draw_week(f, chunks[0], app);
    draw_quick_add(f, chunks[1], app);
    draw_projects(f, chunks[2], app);
    draw_entries(f, chunks[3], app);
    draw_footer(f, chunks[4], app);
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
        .map(|p| p.name.clone())
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
        let mut rows: Vec<(&str, i64)> = active
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    app.snapshot
                        .week
                        .per_project
                        .get(&p.id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows.into_iter()
            .map(|(name, ms)| {
                let selected = app
                    .current_project()
                    .map(|p| p.name == name)
                    .unwrap_or(false);
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
    let project_name = |id: &str| -> String {
        app.snapshot
            .projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string())
    };

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
                let style = if i == app.selected {
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
                    Span::raw(e.label().to_string()),
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

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let footer: String = match &app.status {
        Some(s) => s.clone(),
        None => "1-4 quick add  a 30m  u undo  d delete  j/k move  h/l project  q quit".to_string(),
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
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

    let res = event_loop(&mut terminal, client).await;

    // Always restore the terminal, even if the loop failed, or the user is
    // left with a broken shell.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: Client,
) -> Result<()> {
    let mut app = App {
        client,
        snapshot: Snapshot::default(),
        selected: 0,
        project_cursor: 0,
        last_quick_add_id: None,
        status: None,
        should_quit: false,
    };
    app.refresh().await;

    let mut last_poll = Instant::now();
    while !app.should_quit {
        terminal.draw(|f| draw(f, &app))?;

        if event::poll(POLL)? {
            if let Event::Key(key) = event::read()? {
                // Windows/GTK emit both press and release; acting on both would
                // double every command.
                if key.kind == KeyEventKind::Press {
                    app.on_key(key).await;
                }
            }
        }

        if last_poll.elapsed() >= TICK {
            app.refresh().await;
            last_poll = Instant::now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timetrack_proto::{EntrySource, EntryView};

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
        let mut snap = Snapshot::default();
        snap.projects = vec![project("p1", "Work", false), project("p2", "Old", true)];
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
        let mut snap = Snapshot::default();
        // Newest first, as the service sends them.
        snap.entries = vec![
            entry("e3", EntrySource::QuickAdd, 2_000),
            entry("e2", EntrySource::Manual, 1_000),
            entry("e1", EntrySource::QuickAdd, 0),
        ];
        // Entries are newest first, so e3 is the most recent quick add.
        assert_eq!(last_quick_add(&snap).as_deref(), Some("e3"));
    }

    #[test]
    fn there_is_nothing_to_undo_without_a_quick_add() {
        let mut snap = Snapshot::default();
        snap.entries = vec![entry("e1", EntrySource::Manual, 0)];
        assert_eq!(last_quick_add(&snap), None);
    }

    #[test]
    fn selection_clamps_to_a_shrunk_list() {
        // Simulates the guard in App::refresh: deleting the last entry while
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
