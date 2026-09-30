/* gui/app.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The main window: status, entry list, and the bridge to the service.
//!
//! GPUI is single-threaded and owns the UI, but zbus is async, so the two meet
//! across a thread boundary: a background thread owns an async-io runtime and the
//! bus connection and forwards snapshots over a channel, while the UI drains
//! that channel during `render`. The view holds no authoritative state — every
//! pixel it draws comes from a `Snapshot` the service produced, which is what
//! keeps the GUI and the CLI consistent when both are open.

use gpui::prelude::*;
use gpui::{
    AppContext, AsyncApp, Context, Entity, InteractiveElement, IntoElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px, rgb, rgba,
};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;
use timetrack_proto::Snapshot;

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
    Start(String),
    Stop,
    Cancel,
    Remove(String),
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
            let client = loop {
                if let Ok(c) = timetrack_proto::Client::connect().await {
                    break c;
                }
                // Say what to do about it. "Daemon not running" is a dead
                // end for a user; this names the command that fixes it.
                let _ = tx.send(Msg::Error(
                    "the timetrack service is not running. Start it with:\n                         timetrack-service &\n                     (or install it to a systemd user unit -- see the README)."
                        .to_string(),
                ));
                async_io::Timer::after(Duration::from_secs(2)).await;
            };

            loop {
                // Drain queued commands first, then refresh once. Batching
                // them means one bus round trip per tick, not per keypress.
                loop {
                    match action_rx.try_recv() {
                        Ok(Action::Start(d)) => {
                            report(&tx, client.start(&d).await.map(|_| ()).map_err(|e| e.to_string()))
                        }
                        Ok(Action::Stop) => {
                            report(&tx, client.stop().await.map(|_| ()).map_err(|e| e.to_string()))
                        }
                        Ok(Action::Cancel) => {
                            report(&tx, client.cancel().await.map(|_| ()).map_err(|e| e.to_string()))
                        }
                        Ok(Action::Remove(id)) => {
                            report(&tx, client.remove(&id).await.map_err(|e| e.to_string()))
                        }
                        Ok(Action::StartService) => {
                            // Calling the name is what triggers D-Bus
                            // activation; `Client::connect` just asks for a
                            // proxy, so make a real call to force the bus to
                            // start the service if it can.
                            report(&tx, client.snapshot().await.map(|_| ()).map_err(|e| e.to_string()))
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

pub struct TimetrackView {
    rx: Receiver<Msg>,
    actions: Sender<Action>,
    snapshot: Snapshot,
    status: Option<String>,
    selected: usize,
    /// Entity handle, used to attach the start-service button's click handler
    /// from inside `render` (which only has `&self`).
    footer_entity: Option<Entity<Self>>,
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
            footer_entity: None,
        };
        view.schedule_poll(cx);
        view
    }

    /// Re-render roughly twice a second so the running clock advances smoothly
    /// and a queued service snapshot shows up promptly.
    ///
    /// `cx.spawn` hands the future a `WeakEntity`, so the entity is not kept
    /// alive by its own timer task; if the window closes the upgrade fails and
    /// the loop ends.
    fn schedule_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
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
                    // The list can shrink under the cursor when an entry is
                    // removed, which would otherwise index out of bounds.
                    if self.selected >= self.snapshot.entries.len() {
                        self.selected = self.snapshot.entries.len().saturating_sub(1);
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

    /// Handle a key press. Returns false when the app should quit.
    fn on_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        let Some(ch) = ev.keystroke.key_char.as_deref() else {
            return true;
        };
        match ch {
            "q" => return false,
            " " => {
                if self.snapshot.running().is_some() {
                    self.send(Action::Stop);
                } else {
                    self.send(Action::Start(String::new()));
                }
            }
            "s" => self.send(Action::Stop),
            "c" => self.send(Action::Cancel),
            "d" => {
                if let Some(e) = self.snapshot.entries.get(self.selected) {
                    let id = e.id.clone();
                    self.send(Action::Remove(id));
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

    /// Whether the service has not been seen yet, in which case the window
    /// offers to start it instead of just complaining.
    fn service_missing(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.contains("not running"))
    }

    /// Ask the service thread to try starting the service for us.
    fn try_start_service(&self) {
        self.send(Action::StartService);
    }

    fn footer(&self) -> String {
        self.status.clone().unwrap_or_else(|| {
            "space start/stop · c cancel · d delete · j/k move · q quit".to_string()
        })
    }
}

impl Render for TimetrackView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drain();
        self.schedule_poll(cx);

        let now = timetrack_core::now_ms();
        let running = self.snapshot.running().cloned();
        let this = cx.entity();
        self.footer_entity = Some(this.clone());

        div()
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
            .child(self.render_status(running.as_ref(), now))
            .child(self.render_entries(now))
            .child(self.render_footer())
    }
}

impl TimetrackView {
    fn render_status(
        &self,
        running: Option<&timetrack_proto::EntryView>,
        now: i64,
    ) -> impl IntoElement {
        let elapsed = running.map(|e| e.duration_ms(now)).unwrap_or(0);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_6()
            .py_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(if running.is_some() {
                                0x2ea043ff
                            } else {
                                0x3a3a4aff
                            }))
                            .text_color(rgb(0xffffff))
                            .child(if running.is_some() { " RUNNING" } else { " IDLE" }),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .text_color(rgb(0xffffff))
                            .child(timetrack_core::format_duration(elapsed)),
                    ),
            )
            .child(
                div().text_sm().text_color(rgb(0x9a9ab0)).child(format!(
                    "total {}   ·   {} entries",
                    timetrack_core::format_duration(self.snapshot.total_ms),
                    self.snapshot.entries.len()
                )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xc8c8d8))
                    .child(
                        running
                            .map(|e| e.label().to_string())
                            .unwrap_or_else(|| "nothing running".to_string()),
                    ),
            )
    }

    fn render_entries(&self, now: i64) -> impl IntoElement {
        if self.snapshot.entries.is_empty() {
            return div()
                .flex_1()
                .px_6()
                .child(
                    div()
                        .mt_4()
                        .text_color(rgb(0x6a6a80))
                        .child("No entries yet — press space to start a timer."),
                )
                .into_any_element();
        }

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
                .child(div().w_4().text_color(rgb(0x2ea043)).child(if e.is_running() { "*" } else { " " }))
                .child(
                    div()
                        .w(px(110.))
                        .text_color(rgb(0x7fd3ff))
                        .child(timetrack_core::format_duration(e.duration_ms(now))),
                )
                .child(div().w(px(50.)).text_color(rgb(0x6a6a80)).child(e.id.clone()))
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(0xe6e6f0))
                        .child(e.label().to_string()),
                )
        });

        div()
            .id("entries")
            .flex_1()
            .overflow_y_scroll()
            .child(
                div()
                    .px_6()
                    .pt_2()
                    .pb_1()
                    .text_sm()
                    .text_color(rgb(0x7a7a90))
                    .child("ENTRIES (NEWEST FIRST)"),
            )
            .child(div().flex().flex_col().children(rows))
            .into_any_element()
    }

    fn render_footer(&self) -> impl IntoElement {
        if self.service_missing() {
            let Some(this) = self.footer_entity.clone() else {
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
