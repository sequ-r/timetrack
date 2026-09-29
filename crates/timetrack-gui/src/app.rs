/* gui/app.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The main window: status, entry list, and the bridge to the service.
//!
//! GPUI is single-threaded and owns the UI, but zbus is async, so the two meet
//! through `cx.spawn`: a gpui async task drives the bus connection and pushes
//! snapshots back with `cx.update`. The view holds no authoritative state —
//! everything it draws comes from a `Snapshot` the service produced, which is
//! what keeps the GUI and the CLI consistent when both are open.

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Render, Window, WindowHandle, div, prelude::*, px, rgb, rgb_u8,
};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};
use timetrack_proto::Snapshot;

/// How often the view redraws while a timer is running.
const TICK: Duration = Duration::from_millis(250);

/// How often the service is polled.
const REFRESH: Duration = Duration::from_millis(1000);

/// Messages from the background service thread.
enum Msg {
    Snapshot(Box<Snapshot>),
    Error(String),
}

/// Owns a tokio runtime and the bus connection on its own thread, and forwards
/// snapshots to the UI thread over a channel.
fn spawn_service_thread(
    actions: std::sync::mpsc::Receiver<Action>,
) -> Receiver<Msg> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Msg::Error(format!(
                    "could not start the async runtime: {e}"
                )));
                return;
            }
        };
        rt.block_on(async {
            // Wait for the service, but keep retrying: the user may start it
            // after the GUI, and the window should recover without a restart.
            let client = loop {
                if let Ok(c) = timetrack_proto::Client::connect().await {
                    break c;
                }
                let _ = tx.send(Msg::Error(
                    "waiting for the timetrack service...".to_string(),
                ));
                tokio::time::sleep(Duration::from_secs(2)).await;
            };
            loop {
                // Drain any commands the UI queued, then refresh once. Batching
                // them here means one bus round trip per tick, not per keypress.
                let mut dirty = false;
                loop {
                    match actions.try_recv() {
                        Ok(Action::Start(d)) => report(&tx, client.start(&d).await.map(|_| ())),
                        Ok(Action::Stop) => report(&tx, client.stop().await.map(|_| ())),
                        Ok(Action::Cancel) => report(&tx, client.cancel().await.map(|_| ())),
                        Ok(Action::Remove(id)) => report(&tx, client.remove(&id).await),
                        Err(TryRecvError::Empty) => break,
                        // The UI dropped the channel: nothing left to do.
                        Err(TryRecvError::Disconnected) => return,
                    }
                    dirty = true;
                }
                let _ = dirty;
                match client.snapshot().await {
                    Ok(s) => {
                        if tx.send(Msg::Snapshot(Box::new(s))).is_err() {
                            return; // the UI is gone; stop the thread
                        }
                    }
                    Err(e) => {
                        if tx.send(Msg::Error(e.to_string())).is_err() {
                            return;
                        }
                    }
                }
                tokio::time::sleep(REFRESH).await;
            }
        });
    });
    rx
}

/// Forward a command result to the UI, preserving the service's own wording.
fn report<T>(tx: &std::sync::mpsc::Sender<Msg>, r: Result<T>) {
    if let Err(e) = r {
        let _ = tx.send(Msg::Error(e.to_string()));
    }
}

pub struct TimetrackView {
    rx: Receiver<Msg>,
    snapshot: Snapshot,
    status: Option<String>,
    selected: usize,
    /// When the current snapshot arrived, so the clock can be interpolated
    /// between polls instead of visibly stuttering once a second.
    last_poll: Instant,
    /// Actions requested by the UI, executed by the service thread.
    actions: std::sync::mpsc::Sender<Action>,
}

/// A command the UI wants performed. The service thread owns the bus
/// connection, so the UI cannot issue D-Bus calls itself.
#[derive(Debug, Clone)]
pub enum Action {
    Start(String),
    Stop,
    Cancel,
    Remove(String),
}

impl TimetrackView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (actions_tx, actions_rx) = std::sync::mpsc::channel();
        let view = TimetrackView {
            rx: spawn_service_thread(actions_rx),
            snapshot: Snapshot::default(),
            status: None,
            selected: 0,
            last_poll: Instant::now(),
            actions: actions_tx,
        };
        view.schedule_tick(cx);
        view
    }

    /// Keep a redraw coming so the running clock advances smoothly.
    fn schedule_tick(self: &Entity<Self>, cx: &mut Context<Self>) {
        let this = self.clone();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(TICK)
                    .await;
                this.update(cx, |view, _cx| {
                    view.last_poll = Instant::now();
                });
            }
        })
        .detach();
    }

    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Snapshot(s)) => {
                    self.snapshot = *s;
                    self.status = None;
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

    /// Send a command to the service thread.
    fn send(&self, a: Action) {
        let _ = self.actions.send(a);
    }

    /// Handle a key. Returns false when the key means "quit".
    fn on_key(&mut self, key: &gpui::KeyEvent, _cx: &mut Context<Self>) -> bool {
        use gpui::KeyCode;
        match key.keystroke.key_char {
            Some('q') => return false,
            Some(' ') => {
                if self.snapshot.running().is_some() {
                    self.send(Action::Stop);
                } else {
                    self.send(Action::Start(String::new()));
                }
            }
            Some('s') => self.send(Action::Stop),
            Some('c') => self.send(Action::Cancel),
            Some('d') => {
                if let Some(e) = self.snapshot.entries.get(self.selected) {
                    let id = e.id.clone();
                    self.send(Action::Remove(id));
                }
            }
            Some('j') | Some('k') => {
                let len = self.snapshot.entries.len();
                match key.keystroke.key_char {
                    Some('j') if self.selected + 1 < len => self.selected += 1,
                    Some('k') => self.selected = self.snapshot.entries.saturating_sub(1),
                    _ => {}
                }
            }
            _ => {
                let _ = key.code;
            }
        }
        true
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
        self.schedule_tick(cx);

        let now = timetrack_core::now_ms();
        let running = self.snapshot.running().cloned();

        let this = cx.entity();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x11111b))
            .text_color(rgb(0xe6e6f0))
            .track_focus(&this)
            .on_key_down(move |_ev, _w, cx| {
                // The view cannot mutate itself from inside a closure, so the
                // key is handled by its own entity.
                this.update(cx, |view, cx| {
                    if !view.on_key(&_ev, cx) {
                        cx.quit();
                    }
                })
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
                            .bg(if running.is_some() {
                                rgb_u8(0x2ea043, 0xff)
                            } else {
                                rgb_u8(0x3a3a4a, 0xff)
                            })
                            .text_color(rgb(0xffffff))
                            .child(if running.is_some() {
                                " RUNNING"
                            } else {
                                " IDLE"
                            }),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .text_color(rgb(0xffffff))
                            .child(timetrack_core::format_duration(elapsed)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9a9ab0))
                    .child(format!(
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
        let header = div()
            .px_6()
            .pt_2()
            .pb_1()
            .text_sm()
            .text_color(rgb(0x7a7a90))
            .child("ENTRIES (NEWEST FIRST)");

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
            let marker = if e.is_running() { "*" } else { " " };
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_1()
                .bg(if selected {
                    rgb_u8(0x2a2a3a, 0xff)
                } else {
                    rgb_u8(0x000000, 0x00)
                })
                .child(div().w_4().text_color(rgb(0x2ea043)).child(marker))
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
            .flex_1()
            .overflow_y_scroll()
            .child(header)
            .child(div().flex().flex_col().children(rows))
            .into_any_element()
    }

    fn render_footer(&self) -> impl IntoElement {
        div()
            .px_6()
            .py_2()
            .text_sm()
            .text_color(rgb(0x6a6a80))
            .child(self.footer())
    }
}
