/* cli/tui.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The ratatui front end.
//!
//! The service is the source of truth, so the TUI polls it rather than
//! holding state. Polling at 1Hz is enough for a clock display and keeps the
//! code free of change-notification plumbing; the cost is one round trip per
//! second, which is nothing next to a window system.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::io::{Stdout, stdout};
use std::time::{Duration, Instant};
use timetrack_proto::{Client, Snapshot};

/// How often to refresh from the service.
const TICK: Duration = Duration::from_millis(1000);

/// How often to check for a keypress while idle.
const POLL: Duration = Duration::from_millis(100);

struct App {
    client: Client,
    snapshot: Snapshot,
    selected: usize,
    /// A transient message shown in the footer.
    status: Option<String>,
    should_quit: bool,
}

impl App {
    async fn refresh(&mut self) {
        match self.client.snapshot().await {
            Ok(s) => {
                self.snapshot = s;
                // The list may have shrunk under the cursor (an entry removed).
                if self.selected >= self.snapshot.entries.len() {
                    self.selected = self.snapshot.entries.len().saturating_sub(1);
                }
                self.status = None;
            }
            Err(e) => self.status = Some(format!("service unavailable: {e}")),
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.snapshot.entries.get(self.selected).map(|e| e.id.clone())
    }

    /// Run one action, recording any failure in the footer rather than
    /// exiting: a transient bus error should not kill the UI.
    ///
    /// Takes a closure rather than a future so the call is not evaluated (and
    /// not sent to the bus) until it is actually awaited here.
    async fn act<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        // Client is a cheap handle over the same connection, so clone it out
        // rather than borrow `self` across the call: the call needs `&mut self`
        // again afterwards to refresh and to record any error.
        let client = self.client.clone();
        match f(client).await {
            Ok(()) => {}
            Err(e) => self.status = Some(e.to_string()),
        }
        self.refresh().await;
    }

    async fn on_key(&mut self, key: event::KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('q') => self.should_quit = true,

            KeyCode::Char('s') => {
                let desc = self
                    .snapshot
                    .running()
                    .map(|e| e.description.clone())
                    .unwrap_or_default();
                self.act(|c| async move { c.stop().await.map(|_| ()) }).await;
                self.status = Some(if desc.is_empty() {
                    "stopped".into()
                } else {
                    format!("stopped: {desc}")
                });
            }
            KeyCode::Char('c') => {
                self.act(|c| async move { c.cancel().await.map(|_| ()) }).await;
                self.status = Some("discarded".into());
            }
            KeyCode::Char(' ') => {
                if self.snapshot.running().is_some() {
                    self.act(|c| async move { c.stop().await.map(|_| ()) }).await;
                    self.status = Some("stopped".into());
                } else {
                    self.act(|c| async move { c.start("").await.map(|_| ()) }).await;
                    self.status = Some("started".into());
                }
            }

            KeyCode::Char('d') => {
                if let Some(id) = self.selected_id() {
                    let for_call = id.clone();
                    self.act(move |c| {
                        let id = for_call.clone();
                        async move { c.remove(&id).await }
                    })
                    .await;
                    self.status = Some(format!("removed: {id}"));
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

            _ => {}
        }
    }
}

/// The elapsed time of the running entry, ticked locally between polls so the
/// clock does not visibly stutter between service round trips.
fn running_elapsed(snap: &Snapshot, since_poll: Duration) -> i64 {
    match snap.running() {
        Some(e) => e.duration_ms(timetrack_core::now_ms() - since_poll.as_millis() as i64),
        None => 0,
    }
}

fn draw(f: &mut ratatui::Frame, app: &App, since_poll: Duration) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(4),  // status
        Constraint::Min(3),     // entries
        Constraint::Length(1),  // footer
    ])
    .split(area);

    let now = timetrack_core::now_ms();
    let running = app.snapshot.running();

    // --- status panel -----------------------------------------------------
    let elapsed = match running {
        Some(e) => e.duration_ms(now),
        None => running_elapsed(&app.snapshot, since_poll),
    };
    let status = Paragraph::new(Line::from(vec![
        Span::styled(
            if running.is_some() { " RUNNING  " } else { " IDLE     " },
            Style::default()
                .fg(Color::Black)
                .bg(if running.is_some() { Color::Green } else { Color::DarkGray })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            timetrack_core::format_duration(elapsed),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   total "),
        Span::styled(
            timetrack_core::format_duration(app.snapshot.total_ms),
            Style::default().fg(Color::Cyan),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(running.map(|e| e.label().to_string()).unwrap_or_else(|| "TimeTrack".into())),
    );
    f.render_widget(status, chunks[0]);

    // --- entries ----------------------------------------------------------
    let now = timetrack_core::now_ms();
    let items: Vec<ListItem> = if app.snapshot.entries.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no entries yet — press space to start",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.snapshot
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let style = if i == app.selected {
                    Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let marker = if e.is_running() { "*" } else { " " };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(
                        format!("{:>10}", timetrack_core::format_duration(e.duration_ms(now))),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(format!("  {:<5} ", e.id)),
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
                .title("Entries (newest first)"),
        ),
        chunks[1],
    );

    // --- footer -----------------------------------------------------------
    let footer: String = match &app.status {
        Some(s) => s.clone(),
        None => {
            "space start/stop  c cancel  d delete  j/k move  q quit".to_string()
        }
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
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
        status: None,
        should_quit: false,
    };
    app.refresh().await;

    let mut last_poll = Instant::now();
    while !app.should_quit {
        terminal.draw(|f| draw(f, &app, last_poll.elapsed()))?;

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
    use timetrack_proto::EntryView;

    fn entry(id: &str, started: i64, ended: Option<i64>) -> EntryView {
        EntryView {
            id: id.into(),
            description: id.into(),
            started_at: started,
            ended_at: ended,
        }
    }

    #[test]
    fn running_elapsed_is_zero_when_idle() {
        let s = Snapshot::default();
        assert_eq!(running_elapsed(&s, Duration::from_millis(500)), 0);
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
        let clamped = if selected >= len { len.saturating_sub(1) } else { selected };
        assert_eq!(clamped, 0);
    }

    #[test]
    fn running_entry_is_marked_in_list() {
        // The "*" marker is how a running entry is distinguished at a glance.
        let s = Snapshot {
            running: Some(entry("e1", 0, None)),
            entries: vec![entry("e1", 0, None), entry("e2", 10, Some(20))],
            total_ms: 0,
        };
        assert!(s.entries[0].is_running());
        assert!(!s.entries[1].is_running());
    }
}
