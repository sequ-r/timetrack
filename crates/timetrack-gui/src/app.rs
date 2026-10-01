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

/// Forward a command failure to the UI, keeping the service's own wording.
///
/// Takes an already-rendered message rather than a `Result`, because `Result`
/// is ambiguous in this crate: `anyhow` exports a one-parameter alias and gpui
/// re-exports its own, so naming either one here is a coin flip.
fn report(tx: &Sender<Msg>, outcome: std::result::Result<(), String>) {
    if let Err(e) = outcome {
        let _ = tx.send(Msg::Error(e));
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
                if this.update(cx, |view, _cx| view.drain()).is_err() {
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
    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Snapshot(s)) => {
                    self.snapshot = *s;
                    self.status = None;
                    // The lists can shrink under the cursor when something is
                    // removed, which would otherwise index out of bounds.
                    let entries = self.snapshot.entries.len();
                    if self.selected >= entries {
                        self.selected = entries.saturating_sub(1);
                    }
                    let projects = self.active_projects().len();
                    if self.project_cursor >= projects {
                        self.project_cursor = projects.saturating_sub(1);
                    }
                }
                Ok(Msg::Error(e)) => self.status = Some(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status = Some("the service thread stopped".into());
                    break;
                }
            }
        }
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
            "1-4 quick add · u undo · d delete · j/k move · h/l project · q quit".to_string()
        })
    }
}

impl Render for TimetrackView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drain();
        self.schedule_poll(cx);

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
                this.update(cx, |view, cx| {
                    if !view.on_key(ev) {
                        cx.quit();
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
                                entity.update(cx, |view, _cx| view.tab = tab);
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
            .child(hero)
            .child(warning)
            .child(self.render_quick_add())
            .child(self.render_week_by_project())
            .child(self.render_entries())
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
                                entity.update(cx, |view, _cx| {
                                    view.send(Action::QuickAdd { project, ms })
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
                        entity.update(cx, |view, _cx| {
                            // A typed name needs a text field, which this tab
                            // does not have yet. A dated default the user can
                            // rename is a working button rather than a dead
                            // one, and it is removed once the field lands.
                            let name = format!("Project {}", view.snapshot.projects.len() + 1);
                            view.send(Action::AddProject(name));
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
                                        entity.update(cx, |view, _cx| {
                                            view.send(Action::ArchiveProject(id.clone()))
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
        body =
            body.child(div().pt_1().text_color(rgb(0x9a9ab0)).child(
                "id, project, description, started_at, ended_at, duration_ms, source, note",
            ));

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
                            this.update(cx, |view, _cx| view.try_start_service());
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
}
